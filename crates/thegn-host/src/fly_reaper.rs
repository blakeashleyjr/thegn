//! Fly leak-reaper — the flip side of the Fly ledger's intent-before-create
//! record (`crate::fly` writes it to `vps::registry` with `provider = "fly"`).
//! Fly is NOT a `VpsKind`, so [`crate::vps_reaper`] doesn't cover it; on the
//! hydration cadence this reconciles the Fly ledger against Fly:
//!
//! - a record past the env's `max_lifetime_secs` ⇒ destroy the app (the hard
//!   spend ceiling — a running machine bills);
//! - a `creating` record older than [`CREATING_STALE_SECS`] ⇒ the create crashed
//!   between the intent write and finalize: best-effort destroy + drop it.
//!
//! A `ready` record under the lifetime ceiling is deliberately left alone. If
//! its Fly app was destroyed out-of-band the record is a harmless, non-billing
//! stale IP-cache entry that the next attach re-resolves; reconciling it would
//! cost a per-record Fly API probe (Fly exposes no cheap label-scoped app list
//! the way the VPS providers do), which isn't worth it on the reaper's hot
//! path. Unlike [`crate::vps_reaper`], this reaper never lists the Fly
//! inventory — so it can only reconcile records it already has a reason to
//! touch (lifetime / stale-creating).
//!
//! Runs from the hydration thread ([`tick`] self-throttles to [`TICK_INTERVAL`]);
//! network work runs on its own spawned thread.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
            let result = complete_reap(
                crate::provider_factory::fly_provider_for(pc, &rec.name),
                |provider| {
                    crate::agent::block_on_provider(|| async {
                        provider.destroy_remote_only(&rec.name).await
                    })
                },
                || crate::remote_enqueue_auth::revoke_for_sandbox(pc, &rec.name, None).map(|_| ()),
                |provider| provider.retire_destroyed(&rec.name),
            );
            if let Err(error) = result {
                thegn_core::msg::warn(&format!(
                    "fly reaper: lifecycle for {} failed: {error:#}; ownership records remain for the next pass",
                    rec.name
                ));
            }
        }
        // A `ready` record under the lifetime ceiling is left alone; a machine
        // destroyed out-of-band leaves only a harmless (non-billing) stale record
        // that the next attach re-resolves — not worth an extra API call here.
    }
}

/// Ordered fail-closed lifecycle: provider capability, route-token revocation,
/// remote deletion, then local ownership retirement. A revocation failure must
/// never leave a live remote with its return route, and only the final closure
/// may remove machine/custody records.
fn complete_reap<P>(
    provider: Option<P>,
    mut destroy_remote: impl FnMut(&P) -> Result<()>,
    mut revoke_route_token: impl FnMut() -> Result<()>,
    mut retire_ownership: impl FnMut(&P) -> Result<()>,
) -> Result<()> {
    let provider = provider.context("provider credentials or managed identity unavailable")?;
    revoke_route_token().context("route-to-host credential revocation failed")?;
    destroy_remote(&provider).context("remote destroy failed")?;
    retire_ownership(&provider).context("local ownership retirement failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn run_with_failure(fail: Option<&str>) -> (Result<()>, Vec<&'static str>) {
        let calls = RefCell::new(Vec::new());
        let result = complete_reap(
            (fail != Some("provider")).then_some(()),
            |_| {
                calls.borrow_mut().push("destroy");
                anyhow::ensure!(fail != Some("destroy"), "injected destroy failure");
                Ok(())
            },
            || {
                calls.borrow_mut().push("revoke");
                anyhow::ensure!(fail != Some("revoke"), "injected revoke failure");
                Ok(())
            },
            |_| {
                calls.borrow_mut().push("retire");
                anyhow::ensure!(fail != Some("retire"), "injected retirement failure");
                Ok(())
            },
        );
        (result, calls.into_inner())
    }

    #[test]
    fn ownership_is_retired_only_after_token_revocation_and_remote_destroy() {
        let (result, calls) = run_with_failure(None);
        result.unwrap();
        assert_eq!(calls, ["revoke", "destroy", "retire"]);
    }

    #[test]
    fn every_failure_preserves_the_ownership_retirement_record() {
        for (failure, expected) in [
            ("provider", vec![]),
            ("revoke", vec!["revoke"]),
            ("destroy", vec!["revoke", "destroy"]),
            ("retire", vec!["revoke", "destroy", "retire"]),
        ] {
            let (result, calls) = run_with_failure(Some(failure));
            assert!(result.is_err(), "{failure} must surface");
            assert_eq!(calls, expected, "unexpected lifecycle after {failure}");
            if failure != "retire" {
                assert!(!calls.contains(&"retire"));
            }
        }
    }
}
