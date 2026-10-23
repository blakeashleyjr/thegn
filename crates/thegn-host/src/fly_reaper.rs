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
//! Records stamped for another provider account, or whose account cannot be
//! computed, are quarantined before any provider call.
//!
//! Runs from the hydration thread ([`tick`] self-throttles to [`TICK_INTERVAL`]);
//! network work runs on its own spawned thread. A pass is single-flight across
//! processes per provider kind.

use std::sync::atomic::{AtomicBool, Ordering};
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
    static LEDGER_WARNED: AtomicBool = AtomicBool::new(false);
    let envs: Vec<EnvProviderConfig> = cfg
        .env
        .values()
        .filter(|e| e.provider.provider.trim() == "fly")
        .map(|e| e.provider.clone())
        .collect();
    if envs.is_empty() {
        return;
    }
    let view = match registry::list_checked() {
        Ok(view) => {
            reset_ledger_warning(&LEDGER_WARNED);
            view
        }
        Err(error) => {
            if should_warn_ledger_error(&LEDGER_WARNED) {
                thegn_core::msg::warn(&format!(
                    "fly reaper: skipped fly: ledger unreadable ({error:#}); no provider or ledger actions attempted"
                ));
            }
            return;
        }
    };
    if !has_fly_work(&view) {
        return;
    }
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < TICK_INTERVAL) {
            IN_FLIGHT.store(false, Ordering::Release);
            return;
        }
        *last = Some(Instant::now());
    }
    let spawned = std::thread::Builder::new()
        .name("thegn-fly-reap".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            struct FlightGuard;
            impl Drop for FlightGuard {
                fn drop(&mut self) {
                    IN_FLIGHT.store(false, Ordering::Release);
                }
            }
            let _guard = FlightGuard;
            reap(&envs);
        });
    if let Err(error) = spawned {
        IN_FLIGHT.store(false, Ordering::Release);
        tracing::warn!(target: "thegn::lifecycle", %error, "fly reaper thread could not start");
    }
}

/// The ledger lacks an environment/account binding. Refuse ambiguous policies
/// before provider construction, inventory access or local ledger retirement.
fn reap(envs: &[EnvProviderConfig]) {
    let mut kept = crate::reaper_policy::KeptReport::default();
    let lease = match crate::reaper_policy::try_reaper_lease("fly") {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            tracing::debug!(target: "thegn::lifecycle", "another thegn process is reconciling fly");
            return;
        }
        Err(reason) => {
            kept.keep(
                "fly",
                format!(
                    "fly reaper: skipped fly: {reason}; no provider or ledger actions attempted"
                ),
            );
            kept.publish("fly");
            return;
        }
    };
    let _lease = lease;
    if let Err(reason) =
        crate::reaper_policy::with_unambiguous(envs, |pc| reap_unambiguous(pc, &mut kept))
    {
        kept.keep("fly", format!(
            "fly reaper: quarantined provider kind: {reason}; no provider or ledger actions attempted"
        ));
    }
    kept.publish("fly");
}

fn reap_unambiguous(pc: &EnvProviderConfig, kept: &mut crate::reaper_policy::KeptReport) {
    let view = match registry::list_checked() {
        Ok(view) => view,
        Err(error) => {
            kept.keep("fly", format!(
                "fly reaper: skipped fly: ledger unreadable ({error:#}); no provider or ledger actions attempted"
            ));
            return;
        }
    };
    for name in &view.unreadable {
        kept.keep(name, format!(
            "fly reaper: ledger record {name} unreadable (provider unknown); reconcile ownership; no deletion attempted"
        ));
    }
    let now = thegn_core::util::now();
    for rec in view.records.into_iter().filter(|r| r.provider == "fly") {
        // Legacy created_at is local intent/finalization time, not provider
        // machine age. Never promote it into destructive age evidence.
        if pc.max_lifetime_secs == 0 && rec.state != "creating" {
            continue;
        }
        if pc.max_lifetime_secs > thegn_core::time_policy::MAX_DURATION_SECS {
            kept.keep(&rec.name, format!(
                "fly reaper: quarantined {}: unsupported lifetime policy; repair duration configuration; no deletion attempted",
                rec.name
            ));
            continue;
        }
        let Some(provider) = crate::provider_factory::fly_provider_for(pc, &rec.name) else {
            kept.keep(&rec.name, format!(
                "fly reaper: quarantined {}: managed identity or credentials unavailable; no deletion attempted",
                rec.name
            ));
            continue;
        };
        if let Some(reason) = account_quarantine(&rec, &provider.account()) {
            kept.keep(
                &rec.name,
                format!(
                    "fly reaper: quarantined {}: {reason}; no deletion attempted",
                    rec.name
                ),
            );
            continue;
        }
        let created = match crate::agent::block_on_provider(|| async {
            provider
                .reaper_creation_time(&rec.name, &rec.instance_id)
                .await
        }) {
            Ok(created) => created,
            Err(error) => {
                kept.keep(&rec.name, format!(
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
                kept.keep(
                    &rec.name,
                    format!(
                        "fly reaper: quarantined {}: {reason}; no deletion attempted",
                        rec.name
                    ),
                );
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
            kept.keep(&rec.name, format!(
                "fly reaper: lifecycle for {} failed: {error:#}; ownership records remain for the next pass",
                rec.name
            ));
        }
        // A `ready` record under the lifetime ceiling is left alone; a machine
        // destroyed out-of-band leaves only a harmless (non-billing) stale record
        // that the next attach re-resolves — not worth an extra API call here.
    }
}

fn account_quarantine(record: &registry::VpsRecord, ours: &Result<String>) -> Option<String> {
    match ours {
        Err(error) => Some(format!("provider account unavailable ({error:#})")),
        Ok(ours) if !crate::vps_reaper::record_account_matches(record, ours) => Some(
            "the ledger record belongs to another provider account; reconcile ownership"
                .to_string(),
        ),
        Ok(_) => None,
    }
}

fn has_fly_work(view: &registry::LedgerView) -> bool {
    view.records.iter().any(|record| record.provider == "fly") || !view.unreadable.is_empty()
}

fn should_warn_ledger_error(warned: &AtomicBool) -> bool {
    !warned.swap(true, Ordering::AcqRel)
}

fn reset_ledger_warning(warned: &AtomicBool) {
    warned.store(false, Ordering::Release);
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

    fn account_record(account: &str) -> registry::VpsRecord {
        registry::VpsRecord {
            name: "app".to_string(),
            provider: "fly".to_string(),
            state: "ready".to_string(),
            instance_id: String::new(),
            ip: String::new(),
            created_at: 1,
            owner: String::new(),
            account: account.to_string(),
        }
    }

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
    fn account_gate_lets_legacy_and_same_account_records_proceed() {
        assert_eq!(
            account_quarantine(&account_record(""), &Ok("acct1-a".into())),
            None
        );
        assert_eq!(
            account_quarantine(&account_record("acct1-a"), &Ok("acct1-a".into())),
            None
        );
    }

    #[test]
    fn account_gate_quarantines_another_accounts_record() {
        let reason = account_quarantine(&account_record("acct1-a"), &Ok("acct1-b".into()))
            .expect("different accounts must be quarantined");
        assert!(reason.contains("another provider account"));
    }

    #[test]
    fn account_gate_quarantines_when_the_account_is_unavailable() {
        let reason = account_quarantine(
            &account_record(""),
            &Err(anyhow::anyhow!("authority unavailable")),
        )
        .expect("unavailable account must be quarantined");
        assert!(reason.contains("provider account unavailable"));
        assert!(reason.contains("authority unavailable"));
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
    fn checked_ledger_unreadable_entry_is_reported() {
        let view = registry::LedgerView {
            records: Vec::new(),
            unreadable: vec!["broken".into()],
        };
        assert!(has_fly_work(&view));
    }

    #[test]
    fn checked_ledger_error_warns_once_until_a_successful_listing() {
        let warned = AtomicBool::new(false);
        assert!(should_warn_ledger_error(&warned));
        assert!(!should_warn_ledger_error(&warned));

        reset_ledger_warning(&warned);

        assert!(should_warn_ledger_error(&warned));
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
