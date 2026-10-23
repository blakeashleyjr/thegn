//! Tag-scoped VPS orphan reaper — the flip side of `vps::registry`'s
//! intent-before-create ledger. A leaked VPS bills forever (no free suspended
//! state), so on a slow cadence we reconcile the provider's live, label-scoped
//! instance list against the ledger:
//!
//! - instance labeled ours (`tg-ctl = <this controller id>`) with NO ledger
//!   record and older than [`ORPHAN_AGE_SECS`] ⇒ destroy (a crash between POST
//!   and the ledger finalize, or a record lost out-of-band);
//! - legacy `tg-host`-only orphans are quarantined because their owner is
//!   ambiguous;
//! - ledger record stuck `creating` past [`CREATING_STALE_SECS`] with no live
//!   instance ⇒ drop the record (the POST never landed);
//! - `ready` record whose instance is gone ⇒ drop the record (destroyed
//!   out-of-band — heals the attach bridge's IP cache);
//! - records stamped for another provider account are quarantined and retained;
//! - instance older than the env's `max_lifetime_secs` (when set) ⇒ destroy
//!   (the hard spend ceiling); bake sources also expire at
//!   [`BAKE_MAX_AGE_SECS`] even when that policy is disabled.
//!
//! Runs from the hydration thread ([`tick`] self-throttles to
//! [`TICK_INTERVAL`]) and does all network work on its own spawned thread. A
//! pass is single-flight across processes per provider kind.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thegn_core::config::Config;
use thegn_core::store::PoolStore;
use thegn_svc::vps::{self, registry};

const TICK_INTERVAL: Duration = Duration::from_secs(300);
/// Mirrors the warm-pool stale-provisioning threshold (`reconcile_pool`).
const ORPHAN_AGE_SECS: u64 = 20 * 60;
const CREATING_STALE_SECS: u64 = 10 * 60;
/// Two 1,800 s bake steps plus 180 s to wait for running, 120 s to power off,
/// and 600 s to snapshot take about 75 minutes. Three hours is more than twice
/// that worst case; a bake source past this age is a leak by construction.
const BAKE_MAX_AGE_SECS: u64 = 3 * 60 * 60;

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
    let spawned = thegn_core::thread_name::try_spawn_named("thegn-vps-reap", move || {
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
        tracing::warn!(target: "thegn::lifecycle", %error, "vps reaper thread could not start");
    }
}

/// Reconcile each provider kind only when its environment account references
/// and lifetime policy agree. The ledger cannot resolve which env minted it.
fn reap(envs: &[(String, thegn_core::config::EnvProviderConfig)]) {
    let kinds: std::collections::BTreeSet<&str> =
        envs.iter().map(|(_, pc)| pc.provider.trim()).collect();
    for kind in kinds {
        let mut kept = crate::reaper_policy::KeptReport::default();
        let lease = match crate::reaper_policy::try_reaper_lease(kind) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                tracing::debug!(target: "thegn::lifecycle", "another thegn process is reconciling {kind}");
                continue;
            }
            Err(reason) => {
                kept.keep(kind, format!(
                    "vps reaper: skipped {kind}: {reason}; no provider or ledger actions attempted"
                ));
                kept.publish(kind);
                continue;
            }
        };
        let _lease = lease;
        if let Err(reason) = crate::reaper_policy::with_unambiguous(
            envs.iter()
                .filter(|(_, pc)| pc.provider.trim() == kind)
                .map(|(_, pc)| pc),
            |pc| reap_unambiguous(pc, &mut kept),
        ) {
            kept.keep(kind, format!(
                "vps reaper: quarantined {kind}: {reason}; no provider or ledger actions attempted"
            ));
        }
        kept.publish(kind);
    }
}

fn reap_unambiguous(
    pc: &thegn_core::config::EnvProviderConfig,
    kept: &mut crate::reaper_policy::KeptReport,
) {
    let kind = pc.provider.trim();
    // The outer gate permits exactly one account/policy per provider kind.
    // Failed or unavailable inventory must preserve all ownership records.
    let Some(probe) = crate::provider_factory::vps_provider_for(pc, "reaper-probe") else {
        return;
    };
    let ours_account = match probe.account() {
        Ok(account) => account,
        Err(error) => {
            kept.keep(kind, format!(
                "vps reaper: skipped {kind}: provider account unavailable ({error:#}); no provider or ledger actions attempted"
            ));
            return;
        }
    };
    let view = match registry::list_checked() {
        Ok(view) => view,
        Err(error) => {
            kept.keep(kind, format!(
                "vps reaper: skipped {kind}: ledger unreadable ({error:#}); no provider or ledger actions attempted"
            ));
            return;
        }
    };
    let ours_ctl = match vps::controller::controller_id() {
        Ok(id) => id,
        Err(error) => {
            kept.keep(kind, format!(
                "vps reaper: skipped {kind}: controller id unavailable ({error:#}); no provider or ledger actions attempted"
            ));
            return;
        }
    };
    let ours = vps::host_label();
    let instances = match crate::agent::block_on_provider(|| async { probe.list_detailed().await })
    {
        Ok(list) => list,
        Err(error) => {
            kept.keep(
                kind,
                format!("vps reaper: skipped {kind}: provider inventory unavailable ({error:#}); no provider or ledger actions attempted"),
            );
            return;
        }
    };
    let now = thegn_core::util::now();
    let mut live_anywhere = std::collections::HashSet::new();
    for inst in &instances {
        let class = vps::controller::classify_ownership(&inst.labels, &ours_ctl, &ours);
        let state = ledger_state_for(&view, &inst.name);
        if counts_as_live(class, state) {
            live_anywhere.insert(inst.name.clone());
        }
        match owner_admission(class, state) {
            OwnerAdmission::Proceed => {}
            OwnerAdmission::Quarantine(reason) => {
                kept.keep(
                    &inst.name,
                    format!(
                        "vps reaper: quarantined {}: {reason}; no deletion attempted",
                        inst.name
                    ),
                );
                continue;
            }
            OwnerAdmission::Skip => {
                tracing::debug!(
                    target: "thegn::lifecycle",
                    name = %inst.name,
                    "vps reap: skipping resource not owned by this controller"
                );
                continue;
            }
        }
        let record = match state {
            LedgerState::Recorded(record) => Some(record),
            LedgerState::Unreadable => {
                kept.keep(&inst.name, format!(
                    "vps reaper: quarantined {}: ledger record unreadable; reconcile ownership; no deletion attempted",
                    inst.name
                ));
                continue;
            }
            LedgerState::Absent => None,
        };
        if record.is_some_and(|record| !record_account_matches(record, &ours_account)) {
            kept.keep(&inst.name, format!(
                "vps reaper: quarantined {}: the ledger record belongs to another provider account; reconcile ownership; no deletion attempted",
                inst.name
            ));
            continue;
        }
        if record.is_some_and(|record| !recorded_identity_matches(record, kind, &inst.id))
            && registry::is_bake_name(&inst.name)
            && record
                .is_some_and(|record| record.state == "creating" && record.instance_id.is_empty())
        {
            kept.keep(&inst.name, format!(
                "vps reaper: quarantined {}: bake source with an unfinalized ledger record; destroy it with the provider console or `thegn env image-bake` cleanup; no deletion attempted",
                inst.name
            ));
            continue;
        }
        if record.is_some_and(|record| !recorded_identity_matches(record, kind, &inst.id)) {
            kept.keep(&inst.name, format!(
                "vps reaper: quarantined {}: inventory identity differs from the ledger; reconcile ownership; no deletion attempted",
                inst.name
            ));
            continue;
        }
        let decision = thegn_core::time_policy::resource_expiry(
            now,
            inst.created,
            lifetime_for(&inst.name, pc.max_lifetime_secs),
            orphan_age_for(state),
        );
        if !admit_reap(decision, |reason| {
            kept.keep(&inst.name, format!(
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
                kept.keep(&inst.name, format!(
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
                Err(e) => kept.keep(
                    &inst.name,
                    format!(
                        "vps reaper: destroy {} failed: {e}; will retry next pass",
                        inst.name
                    ),
                ),
            }
        }
    }
    // Only this successfully reconciled kind can retire absent ledger rows.
    let records = cleanup_candidates(view.records, kind, &ours_account);
    cleanup_ledger(&records, &live_anywhere, kept);
}

pub(crate) fn record_account_matches(record: &registry::VpsRecord, ours: &str) -> bool {
    record.account.is_empty() || record.account == ours
}

fn cleanup_candidates(
    records: Vec<registry::VpsRecord>,
    kind: &str,
    ours: &str,
) -> Vec<registry::VpsRecord> {
    records
        .into_iter()
        .filter(|record| record.provider == kind && record_account_matches(record, ours))
        .collect()
}

#[derive(Clone, Copy)]
enum LedgerState<'a> {
    Recorded(&'a registry::VpsRecord),
    Unreadable,
    Absent,
}

#[derive(Debug, PartialEq, Eq)]
enum OwnerAdmission {
    Proceed,
    Quarantine(&'static str),
    Skip,
}

fn owner_admission(class: vps::controller::Ownership, state: LedgerState<'_>) -> OwnerAdmission {
    use vps::controller::Ownership::{Foreign, LegacyHost, Ours, Unlabelled};
    match (class, state) {
        (Ours, _) | (LegacyHost, LedgerState::Recorded(_) | LedgerState::Unreadable) => {
            OwnerAdmission::Proceed
        }
        (LegacyHost, LedgerState::Absent) => OwnerAdmission::Quarantine(
            "legacy tg-host label with no controller id and no ledger record; ownership is ambiguous",
        ),
        (Foreign | Unlabelled, LedgerState::Absent) => OwnerAdmission::Skip,
        (Foreign | Unlabelled, LedgerState::Recorded(_) | LedgerState::Unreadable) => {
            OwnerAdmission::Quarantine(
                "a ledger record names an instance another controller labels",
            )
        }
    }
}

fn counts_as_live(class: vps::controller::Ownership, state: LedgerState<'_>) -> bool {
    matches!(
        class,
        vps::controller::Ownership::Ours | vps::controller::Ownership::LegacyHost
    ) || !matches!(state, LedgerState::Absent)
}

fn lifetime_for(name: &str, configured: u64) -> u64 {
    if registry::is_bake_name(name) {
        match configured {
            0 => BAKE_MAX_AGE_SECS,
            _ => configured.min(BAKE_MAX_AGE_SECS),
        }
    } else {
        configured
    }
}

fn ledger_state_for<'a>(view: &'a registry::LedgerView, name: &str) -> LedgerState<'a> {
    if let Some(record) = view.records.iter().find(|record| record.name == name) {
        LedgerState::Recorded(record)
    } else if view.unreadable.iter().any(|unreadable| unreadable == name) {
        LedgerState::Unreadable
    } else {
        LedgerState::Absent
    }
}

fn orphan_age_for(state: LedgerState<'_>) -> Option<u64> {
    matches!(state, LedgerState::Absent).then_some(ORPHAN_AGE_SECS)
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
fn cleanup_ledger(
    records: &[registry::VpsRecord],
    live: &std::collections::HashSet<String>,
    kept: &mut crate::reaper_policy::KeptReport,
) {
    let now = thegn_core::util::now();
    for rec in records {
        if rec.state == "creating"
            && thegn_core::time_policy::age_seconds(now, rec.created_at).is_none()
        {
            kept.keep(&rec.name, format!(
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
    fn ownership_admission_matrix() {
        use vps::controller::Ownership::{Foreign, LegacyHost, Ours, Unlabelled};

        let record = rec("resource", "ready", 1);
        let absent = LedgerState::Absent;
        let unreadable = LedgerState::Unreadable;
        let recorded = LedgerState::Recorded(&record);

        assert_eq!(owner_admission(Ours, recorded), OwnerAdmission::Proceed);
        assert_eq!(owner_admission(Ours, unreadable), OwnerAdmission::Proceed);
        assert_eq!(owner_admission(Ours, absent), OwnerAdmission::Proceed);
        assert!(counts_as_live(Ours, recorded));
        assert!(counts_as_live(Ours, unreadable));
        assert!(counts_as_live(Ours, absent));

        assert_eq!(
            owner_admission(LegacyHost, recorded),
            OwnerAdmission::Proceed
        );
        assert_eq!(
            owner_admission(LegacyHost, unreadable),
            OwnerAdmission::Proceed
        );
        assert_eq!(
            owner_admission(LegacyHost, absent),
            OwnerAdmission::Quarantine(
                "legacy tg-host label with no controller id and no ledger record; ownership is ambiguous"
            )
        );
        assert!(counts_as_live(LegacyHost, recorded));
        assert!(counts_as_live(LegacyHost, unreadable));
        assert!(counts_as_live(LegacyHost, absent));

        assert_eq!(
            owner_admission(Foreign, recorded),
            OwnerAdmission::Quarantine(
                "a ledger record names an instance another controller labels"
            )
        );
        assert_eq!(
            owner_admission(Foreign, unreadable),
            OwnerAdmission::Quarantine(
                "a ledger record names an instance another controller labels"
            )
        );
        assert_eq!(owner_admission(Foreign, absent), OwnerAdmission::Skip);
        assert!(counts_as_live(Foreign, recorded));
        assert!(counts_as_live(Foreign, unreadable));
        assert!(!counts_as_live(Foreign, absent));

        assert_eq!(
            owner_admission(Unlabelled, recorded),
            OwnerAdmission::Quarantine(
                "a ledger record names an instance another controller labels"
            )
        );
        assert_eq!(
            owner_admission(Unlabelled, unreadable),
            OwnerAdmission::Quarantine(
                "a ledger record names an instance another controller labels"
            )
        );
        assert_eq!(owner_admission(Unlabelled, absent), OwnerAdmission::Skip);
        assert!(counts_as_live(Unlabelled, recorded));
        assert!(counts_as_live(Unlabelled, unreadable));
        assert!(!counts_as_live(Unlabelled, absent));
    }

    #[test]
    fn legacy_label_without_ledger_is_never_an_orphan() {
        match owner_admission(vps::controller::Ownership::LegacyHost, LedgerState::Absent) {
            OwnerAdmission::Quarantine(reason) => assert!(reason.contains("legacy")),
            other => panic!("expected legacy ownership quarantine, got {other:?}"),
        }
    }

    #[test]
    fn bake_lifetime_is_capped_even_when_lifetime_policy_is_off() {
        let bake = "tg-bake-00010203fafbfeff";
        assert_eq!(lifetime_for(bake, 0), BAKE_MAX_AGE_SECS);
        assert_eq!(lifetime_for(bake, 600), 600);
        assert_eq!(lifetime_for(bake, 99_999_999), BAKE_MAX_AGE_SECS);
        assert_eq!(lifetime_for("tg-dev-1", 0), 0);

        let now = 2_000_000_000;
        assert_eq!(
            thegn_core::time_policy::resource_expiry(
                now,
                Some(now - BAKE_MAX_AGE_SECS as i64 - 1),
                lifetime_for(bake, 0),
                orphan_age_for(LedgerState::Recorded(&rec(bake, "ready", now - 1))),
            ),
            thegn_core::time_policy::ResourceExpiry::Expired
        );
        assert_eq!(
            thegn_core::time_policy::resource_expiry(
                now,
                Some(now - BAKE_MAX_AGE_SECS as i64 + 60),
                lifetime_for(bake, 0),
                orphan_age_for(LedgerState::Recorded(&rec(bake, "ready", now - 1))),
            ),
            thegn_core::time_policy::ResourceExpiry::Keep
        );
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
            owner: String::new(),
            account: String::new(),
        }
    }

    #[test]
    fn checked_ledger_unreadable_record_is_never_an_orphan() {
        let record = rec("recorded", "ready", 1);
        let view = registry::LedgerView {
            records: vec![record],
            unreadable: vec!["broken".into()],
        };
        assert!(matches!(
            ledger_state_for(&view, "broken"),
            LedgerState::Unreadable
        ));
        assert!(matches!(
            ledger_state_for(&view, "recorded"),
            LedgerState::Recorded(_)
        ));
        assert!(matches!(
            ledger_state_for(&view, "missing"),
            LedgerState::Absent
        ));
        assert_eq!(orphan_age_for(ledger_state_for(&view, "broken")), None);
        assert_eq!(orphan_age_for(ledger_state_for(&view, "recorded")), None);
        assert_eq!(
            orphan_age_for(ledger_state_for(&view, "missing")),
            Some(ORPHAN_AGE_SECS)
        );
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
    fn account_mismatch_record_is_kept_and_never_matches() {
        let mut record = rec("resource", "ready", 1);
        record.account = "acct1-ours".into();
        assert!(record_account_matches(&record, "acct1-ours"));
        assert!(!record_account_matches(&record, "acct1-other"));
        record.account.clear();
        assert!(record_account_matches(&record, "acct1-other"));
    }

    #[test]
    fn account_cleanup_keeps_other_account_records() {
        let mut other = rec("other", "ready", 1);
        other.account = "acct1-other".into();
        let mut same = rec("same", "ready", 1);
        same.account = "acct1-ours".into();
        let legacy = rec("legacy", "ready", 1);
        let selected = cleanup_candidates(vec![other, legacy, same], "hetzner", "acct1-ours");

        assert_eq!(
            selected
                .iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>(),
            vec!["legacy", "same"]
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
