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
//! Ready records use a read-only Machines inventory reconciliation to verify
//! the exact persisted machine ID, ownership metadata and provider creation time.
//! Legacy local created_at is never promoted to provider age. Unknown or changed
//! inventory remains visible in quarantine, with retry on the next pass. A
//! staged create without authoritative machine identity cannot authorize expiry.
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
const CREATING_STALE_SECS: u64 = 10 * 60;

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
        // Legacy created_at is local intent/finalization time, not provider
        // machine age. Never promote it into destructive age evidence.
        if pc.max_lifetime_secs == 0 && rec.state != "creating" {
            continue;
        }
        if pc.max_lifetime_secs > thegn_core::time_policy::MAX_DURATION_SECS {
            thegn_core::msg::warn(&format!(
                "fly reaper: quarantined {}: unsupported lifetime policy; repair duration configuration; no deletion attempted",
                rec.name
            ));
            continue;
        }
        let Some(provider) = crate::provider_factory::fly_provider_for(pc, &rec.name) else {
            thegn_core::msg::warn(&format!(
                "fly reaper: quarantined {}: managed identity or credentials unavailable; no deletion attempted",
                rec.name
            ));
            continue;
        };
        let created = match crate::agent::block_on_provider(|| async {
            provider
                .reaper_creation_time(&rec.name, &rec.instance_id)
                .await
        }) {
            Ok(created) => created,
            Err(error) => {
                thegn_core::msg::warn(&format!(
                    "fly reaper: quarantined {}: creation time/identity could not be reconciled ({error:#}); no deletion attempted; next pass will retry",
                    rec.name
                ));
                continue;
            }
        };
        let decision = thegn_core::time_policy::resource_expiry(
            now,
            Some(created),
            pc.max_lifetime_secs,
            (rec.state == "creating").then_some(CREATING_STALE_SECS),
        );
        match decision {
            thegn_core::time_policy::ResourceExpiry::Keep => continue,
            thegn_core::time_policy::ResourceExpiry::Quarantine(reason) => {
                thegn_core::msg::warn(&format!(
                    "fly reaper: quarantined {}: {reason}; no deletion attempted",
                    rec.name
                ));
                continue;
            }
            thegn_core::time_policy::ResourceExpiry::Expired => {}
        }
        thegn_core::msg::warn(&format!(
            "fly reaper: destroying {} (verified provider age exceeds lifetime/stale-create policy)",
            rec.name
        ));
        let result = complete_reap(
            decision,
            Some(provider),
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
    decision: thegn_core::time_policy::ResourceExpiry,
    provider: Option<P>,
    mut destroy_remote: impl FnMut(&P) -> Result<()>,
    mut revoke_route_token: impl FnMut() -> Result<()>,
    mut retire_ownership: impl FnMut(&P) -> Result<()>,
) -> Result<()> {
    match decision {
        thegn_core::time_policy::ResourceExpiry::Keep => return Ok(()),
        thegn_core::time_policy::ResourceExpiry::Quarantine(reason) => {
            anyhow::bail!("quarantined: {reason}")
        }
        thegn_core::time_policy::ResourceExpiry::Expired => {}
    }
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
            thegn_core::time_policy::ResourceExpiry::Expired,
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
    fn unknown_age_or_invalid_lifetime_never_reaches_any_lifecycle_action() {
        for (created, lifetime) in [
            (None, 60),
            (Some(i64::MIN), 60),
            (Some(101), 1),
            (Some(1), u64::MAX),
        ] {
            let decision =
                thegn_core::time_policy::resource_expiry(100, created, lifetime, Some(60));
            let calls = std::cell::Cell::new(0);
            let result = complete_reap(
                decision,
                Some(()),
                |_| {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
                || {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
                |_| {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(calls.get(), 0);
        }
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
