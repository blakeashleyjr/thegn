//! Fly leak-reaper — the flip side of the Fly ledger's intent-before-create
//! record (`crate::fly` writes it to `vps::registry` with `provider = "fly"`).
//! Fly is NOT a `VpsKind`, so [`crate::vps_reaper`] doesn't cover it; on the
//! hydration cadence this reconciles the Fly ledger against Fly:
//!
//! - a record past the env's `max_lifetime_secs` ⇒ destroy the app (the hard
//!   spend ceiling — a running machine bills);
//! - a `creating` record older than [`CREATING_STALE_SECS`] ⇒ the create crashed
//!   between the intent write and finalize: best-effort destroy + drop it;
//! - a `ready` record whose app/machine is gone (destroyed out-of-band) ⇒ drop
//!   the record (heals the attach path's assumptions).
//!
//! Runs from the hydration thread ([`tick`] self-throttles to [`TICK_INTERVAL`]);
//! network work runs on its own spawned thread.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use thegn_core::config::{Config, EnvProviderConfig};
use thegn_svc::vps::registry;

const TICK_INTERVAL: Duration = Duration::from_secs(300);
/// Mirrors the VPS reaper's stale-`creating` threshold.
const CREATING_STALE_SECS: i64 = 10 * 60;

/// Throttled entry: schedule one reconcile pass when due. Cheap (and free) when
/// no `provider = "fly"` env is configured or the ledger has no Fly records.
pub fn tick(cfg: &Config) {
    let envs: Vec<EnvProviderConfig> = cfg
        .env
        .values()
        .filter(|e| e.provider.provider.trim() == "fly")
        .map(|e| e.provider.clone())
        .collect();
    if envs.is_empty() {
        return;
    }
    if !registry::list().iter().any(|r| r.provider == "fly") {
        return;
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < TICK_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    std::thread::spawn(move || reap(&envs));
}

/// One reconcile pass over the Fly ledger records. The env's `max_lifetime_secs`
/// ceiling is taken from the first fly env (mirrors the VPS reaper's per-provider
/// treatment; a Fly record doesn't record which env minted it).
fn reap(envs: &[EnvProviderConfig]) {
    use thegn_svc::provider::RemoteProvider;
    let Some(pc) = envs.first() else { return };
    let now = thegn_core::util::now();
    for rec in registry::list().into_iter().filter(|r| r.provider == "fly") {
        let age = now - rec.created_at;
        let over_lifetime = pc.max_lifetime_secs > 0 && age >= pc.max_lifetime_secs as i64;
        let stale_creating = rec.state == "creating" && age >= CREATING_STALE_SECS;

        if over_lifetime || stale_creating {
            let why = if over_lifetime {
                "past max_lifetime_secs"
            } else {
                "stale creating (crashed create?)"
            };
            thegn_core::msg::warn(&format!(
                "fly reaper: destroying {} ({why}, age {}m) — a running machine bills",
                rec.name,
                age / 60
            ));
            if let Some(p) = crate::provider_factory::fly_provider_for(pc, &rec.name)
                && let Err(e) =
                    crate::agent::block_on_provider(|| async { p.destroy(&rec.name).await })
            {
                thegn_core::msg::warn(&format!(
                    "fly reaper: destroy {} failed: {e}; will retry next pass",
                    rec.name
                ));
            }
            // destroy() clears the ledger; ensure it's gone even if the provider
            // couldn't be built (missing token) so it doesn't loop forever.
            registry::remove(&rec.name);
        }
        // A `ready` record under the lifetime ceiling is left alone; a machine
        // destroyed out-of-band leaves only a harmless (non-billing) stale record
        // that the next attach re-resolves — not worth an extra API call here.
    }
}
