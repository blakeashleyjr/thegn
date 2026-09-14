//! Tag-scoped VPS orphan reaper — the flip side of `vps::registry`'s
//! intent-before-create ledger. A leaked VPS bills forever (no free suspended
//! state), so on a slow cadence we reconcile the provider's live, label-scoped
//! instance list against the ledger:
//!
//! - instance labeled ours (`tg-host = <this host>`) with NO ledger record and
//!   older than [`ORPHAN_AGE_SECS`] ⇒ destroy (a crash between POST and the
//!   ledger finalize, or a record lost out-of-band);
//! - ledger record stuck `creating` past [`CREATING_STALE_SECS`] with no live
//!   instance ⇒ drop the record (the POST never landed);
//! - `ready` record whose instance is gone ⇒ drop the record (destroyed
//!   out-of-band — heals the attach bridge's IP cache);
//! - instance older than the env's `max_lifetime_secs` (when set) ⇒ destroy
//!   (the hard spend ceiling).
//!
//! Runs from the hydration thread ([`tick`] self-throttles to
//! [`TICK_INTERVAL`]) and does all network work on its own spawned thread.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use thegn_core::config::Config;
use thegn_core::store::PoolStore;
use thegn_svc::vps::{self, registry};

const TICK_INTERVAL: Duration = Duration::from_secs(300);
/// Mirrors the warm-pool stale-provisioning threshold (`reconcile_pool`).
const ORPHAN_AGE_SECS: u64 = 20 * 60;
const CREATING_STALE_SECS: u64 = 10 * 60;

/// Throttled entry: schedule one reconcile pass when due. Cheap when not due
/// or when no VPS env is configured; network work runs on its own thread.
pub fn tick(cfg: &Config) {
    // Collect the VPS-kind envs first — a host with none configured must pay
    // nothing here (not even the throttle bookkeeping).
    let envs: Vec<(String, thegn_core::config::EnvProviderConfig)> = cfg
        .env
        .iter()
        .filter(|(_, e)| thegn_core::config::vps_provider_kind(&e.provider.provider))
        .map(|(n, e)| (n.clone(), e.provider.clone()))
        .collect();
    if envs.is_empty() {
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

/// Reconcile each provider kind only when its environment account references
/// and lifetime policy agree. The ledger cannot resolve which env minted it.
fn reap(envs: &[(String, thegn_core::config::EnvProviderConfig)]) {
    let kinds: std::collections::BTreeSet<&str> =
        envs.iter().map(|(_, pc)| pc.provider.trim()).collect();
    for kind in kinds {
        if let Err(reason) = crate::reaper_policy::with_unambiguous(
            envs.iter()
                .filter(|(_, pc)| pc.provider.trim() == kind)
                .map(|(_, pc)| pc),
            reap_unambiguous,
        ) {
            thegn_core::msg::warn(&format!(
                "vps reaper: quarantined {kind}: {reason}; no provider or ledger actions attempted"
            ));
        }
    }
}

fn reap_unambiguous(pc: &thegn_core::config::EnvProviderConfig) {
    let ours = vps::host_label();
    let kind = pc.provider.trim();
    // The outer gate permits exactly one account/policy per provider kind.
    // Failed or unavailable inventory must preserve all ownership records.
    let Some(probe) = crate::provider_factory::vps_provider_for(pc, "reaper-probe") else {
        return;
    };
    let instances = match crate::agent::block_on_provider(|| async { probe.list_detailed().await })
    {
        Ok(list) => list,
        Err(error) => {
            tracing::debug!(target: "thegn::lifecycle", %error, "vps reap: list failed");
            return;
        }
    };
    let records = registry::list();
    let now = thegn_core::util::now();
    let mut live_anywhere = std::collections::HashSet::new();
    for inst in instances
        .iter()
        .filter(|i| i.labels.get("tg-host").map(String::as_str) == Some(ours.as_str()))
    {
        live_anywhere.insert(inst.name.clone());
        let record = records.iter().find(|r| r.name == inst.name);
        if record.is_some_and(|record| !recorded_identity_matches(record, kind, &inst.id)) {
            thegn_core::msg::warn(&format!(
                "vps reaper: quarantined {}: inventory identity differs from the ledger; reconcile ownership; no deletion attempted",
                inst.name
            ));
            continue;
        }
        let decision = thegn_core::time_policy::resource_expiry(
            now,
            inst.created,
            pc.max_lifetime_secs,
            record.is_none().then_some(ORPHAN_AGE_SECS),
        );
        if !admit_reap(decision, |reason| {
            thegn_core::msg::warn(&format!(
                "vps reaper: quarantined {}: {reason}; no deletion attempted; next pass will retry",
                inst.name
            ));
        }) {
            continue;
        }
        thegn_core::msg::warn(&format!(
            "vps reaper: destroying {} (verified provider age exceeds lifetime/orphan policy)",
            inst.name
        ));
        if let Some(p) = crate::provider_factory::vps_provider_for(pc, &inst.name) {
            if let Err(error) = crate::remote_enqueue_auth::revoke_for_sandbox(pc, &inst.name, None)
            {
                thegn_core::msg::warn(&format!(
                    "vps reaper: refusing to destroy {} because route-to-host credential revocation failed: {error:#}",
                    inst.name
                ));
                continue;
            }
            use thegn_svc::provider::RemoteProvider;
            match crate::agent::block_on_provider(|| async { p.destroy(&inst.name).await }) {
                // destroy() clears the ledger + known_hosts; also drop any
                // pool row so the warm pool refills. best-effort: the DB is
                // a cache and the next reconcile re-observes.
                Ok(()) => {
                    live_anywhere.remove(&inst.name);
                    if let Ok(db) = thegn_core::db::Db::open() {
                        let _ = db.delete_pool_spare(&inst.name); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                    }
                }
                Err(e) => thegn_core::msg::warn(&format!(
                    "vps reaper: destroy {} failed: {e}; will retry next pass",
                    inst.name
                )),
            }
        }
    }
    // Only this successfully reconciled kind can retire absent ledger rows.
    let records: Vec<_> = records.into_iter().filter(|r| r.provider == kind).collect();
    cleanup_ledger(&records, &live_anywhere);
}

/// A staged intent is not an observed provider identity. It may participate in
/// no-live-instance cleanup, but cannot authorize deletion of a named resource.
fn recorded_identity_matches(record: &registry::VpsRecord, kind: &str, id: &str) -> bool {
    record.provider == kind && !record.instance_id.is_empty() && record.instance_id == id
}

/// The sole time-policy admission gate before any VPS lifecycle action.
fn admit_reap(
    decision: thegn_core::time_policy::ResourceExpiry,
    report: impl FnOnce(&str),
) -> bool {
    match decision {
        thegn_core::time_policy::ResourceExpiry::Expired => true,
        thegn_core::time_policy::ResourceExpiry::Keep => false,
        thegn_core::time_policy::ResourceExpiry::Quarantine(reason) => {
            report(reason);
            false
        }
    }
}

/// Pure decision: should this ledger record be dropped, given the union of
/// names seen live across ALL accounts we reconciled this pass? A record with a
/// live instance anywhere is kept (its VPS may belong to a sibling account of
/// the same kind); otherwise stale `creating` or any `ready` record is dropped.
fn should_drop_record(
    rec: &registry::VpsRecord,
    live: &std::collections::HashSet<String>,
    now: i64,
) -> bool {
    if live.contains(&rec.name) {
        return false;
    }
    let stale_creating = rec.state == "creating"
        && thegn_core::time_policy::age_seconds(now, rec.created_at)
            .is_some_and(|age| age >= CREATING_STALE_SECS);
    let gone_ready = rec.state == "ready";
    stale_creating || gone_ready
}

/// Drop ledger records with no live instance ANYWHERE (across all accounts of
/// the same kind): stale `creating` (the POST never landed) or a `ready` record
/// whose instance is gone out-of-band.
fn cleanup_ledger(records: &[registry::VpsRecord], live: &std::collections::HashSet<String>) {
    let now = thegn_core::util::now();
    for rec in records {
        if rec.state == "creating"
            && thegn_core::time_policy::age_seconds(now, rec.created_at).is_none()
        {
            thegn_core::msg::warn(&format!(
                "vps reaper: quarantined ledger {}: invalid/future intent time; reconcile inventory; record retained",
                rec.name
            ));
        }
        if !should_drop_record(rec, live, now) {
            continue;
        }
        tracing::debug!(
            target: "thegn::lifecycle",
            name = %rec.name, state = %rec.state,
            "vps reap: dropping ledger record with no live instance"
        );
        registry::remove(&rec.name);
        if let Ok(db) = thegn_core::db::Db::open() {
            // best-effort: phantom pool rows must not linger either.
            let _ = db.delete_pool_spare(&rec.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_or_mismatched_record_never_admits_resource_deletion() {
        let mut record = rec("sandbox", "creating", 1);
        for (provider, id) in [
            ("hetzner", ""),
            ("hetzner", "other"),
            ("digitalocean", "id"),
        ] {
            record.provider = provider.into();
            record.instance_id = id.into();
            let mut deletes = 0;
            if recorded_identity_matches(&record, "hetzner", "id") {
                deletes += 1;
            }
            assert_eq!(deletes, 0);
        }
        record.provider = "hetzner".into();
        record.instance_id = "id".into();
        assert!(recorded_identity_matches(&record, "hetzner", "id"));
    }

    #[test]
    fn unknown_or_hostile_time_is_observable_and_never_admitted() {
        for (created, lifetime) in [
            (None, 60),
            (Some(0), 60),
            (Some(i64::MIN), 60),
            (Some(101), 1),
            (Some(1), u64::MAX),
        ] {
            let mut notices = 0;
            let mut destroys = 0;
            let decision =
                thegn_core::time_policy::resource_expiry(100, created, lifetime, Some(60));
            if admit_reap(decision, |_| notices += 1) {
                destroys += 1;
            }
            assert_eq!((notices, destroys), (1, 0));
        }
    }

    fn rec(name: &str, state: &str, created_at: i64) -> registry::VpsRecord {
        registry::VpsRecord {
            name: name.into(),
            provider: "hetzner".into(),
            state: state.into(),
            instance_id: String::new(),
            ip: String::new(),
            created_at,
        }
    }

    #[test]
    fn ready_record_of_a_sibling_account_is_kept() {
        // Two hetzner accounts (personal + work). Account 1's list has none of
        // account 2's instances, but `live` is the UNION across both accounts,
        // so account 2's still-live `ready` VPS is NOT dropped from the ledger.
        let now = 2_000_000_000;
        let mut live = std::collections::HashSet::new();
        live.insert("tg-work-live".to_string()); // seen live under account 2
        let r = rec("tg-work-live", "ready", now - 10_000);
        assert!(
            !should_drop_record(&r, &live, now),
            "a live sibling-account VPS must never be reaped from the ledger"
        );
    }

    #[test]
    fn gone_ready_and_stale_creating_are_dropped() {
        let now = 2_000_000_000;
        let live = std::collections::HashSet::new(); // nothing live anywhere
        // ready but gone out-of-band ⇒ drop.
        assert!(should_drop_record(
            &rec("gone", "ready", now - 10_000),
            &live,
            now
        ));
        // creating older than the stale threshold ⇒ drop.
        assert!(should_drop_record(
            &rec("stuck", "creating", now - CREATING_STALE_SECS as i64 - 1),
            &live,
            now
        ));
        // creating but still fresh ⇒ keep (create may be in flight).
        assert!(!should_drop_record(
            &rec("fresh", "creating", now - 1),
            &live,
            now
        ));
    }
}
