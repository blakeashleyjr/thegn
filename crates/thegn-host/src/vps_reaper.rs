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
const ORPHAN_AGE_SECS: i64 = 20 * 60;
const CREATING_STALE_SECS: i64 = 10 * 60;

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

/// One reconcile pass over the configured VPS envs. Envs sharing an account
/// (same kind + api base) are deduped so the account is listed once.
fn reap(envs: &[(String, thegn_core::config::EnvProviderConfig)]) {
    let ours = vps::host_label();
    let mut seen_accounts: Vec<String> = Vec::new();
    // Names of every instance seen live across ALL accounts this pass. The
    // ledger `ready`-drop must run against this UNION, not one account's list:
    // a `VpsRecord` carries no api_base, so a `ready` record minted by a SECOND
    // account of the same provider kind is absent from any OTHER account's list
    // and would otherwise be dropped while its VPS is still live/billing.
    let mut live_anywhere: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Provider kinds for which EVERY account listed cleanly this pass. Only
    // these are eligible for `ready`-record cleanup: if any account of a kind
    // failed to list (token gone / API error), we can't tell a genuinely-gone
    // record from one whose live instance we simply couldn't see, so we leave
    // that kind's records alone until a clean pass.
    let mut listed_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut failed_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (env_name, pc) in envs {
        let kind = pc.provider.trim().to_string();
        let account = format!("{}|{}", kind, pc.api_base.trim());
        if seen_accounts.contains(&account) {
            continue;
        }
        seen_accounts.push(account);
        // Token unset ⇒ nothing reachable to reap (and nothing could have been
        // created); skip quietly, same as the launch path.
        let Some(probe) = crate::provider_factory::vps_provider_for(pc, "reaper-probe") else {
            failed_kinds.insert(kind.clone());
            continue;
        };
        let instances = match crate::agent::block_on_provider(|| async {
            probe.list_detailed().await
        }) {
            Ok(list) => list,
            Err(e) => {
                tracing::debug!(target: "thegn::lifecycle", error = %e, "vps reap: list failed");
                failed_kinds.insert(kind.clone());
                continue;
            }
        };
        listed_kinds.insert(kind.clone());
        let records = registry::list();
        let now = thegn_core::util::now();

        for inst in instances
            .iter()
            .filter(|i| i.labels.get("tg-host").map(String::as_str) == Some(ours.as_str()))
        {
            live_anywhere.insert(inst.name.clone());
            let record = records.iter().find(|r| r.name == inst.name);
            let age = inst.created.map(|c| now - c).unwrap_or(0);
            let over_lifetime = pc.max_lifetime_secs > 0 && age >= pc.max_lifetime_secs as i64;
            let orphaned = record.is_none() && age >= ORPHAN_AGE_SECS;
            if !(orphaned || over_lifetime) {
                continue;
            }
            let why = if orphaned {
                "not in the local ledger (crashed create?)"
            } else {
                "past max_lifetime_secs"
            };
            thegn_core::msg::warn(&format!(
                "vps reaper: destroying {} ({why}, age {}m) — a VPS bills until destroyed",
                inst.name,
                age / 60
            ));
            if let Some(p) = crate::provider_factory::vps_provider_for(pc, &inst.name) {
                use thegn_svc::provider::RemoteProvider;
                match crate::agent::block_on_provider(|| async { p.destroy(&inst.name).await }) {
                    // destroy() clears the ledger + known_hosts; also drop any
                    // pool row so the warm pool refills. best-effort: the DB is
                    // a cache and the next reconcile re-observes.
                    Ok(()) => {
                        live_anywhere.remove(&inst.name);
                        if let Ok(db) = thegn_core::db::Db::open() {
                            let _ = db.delete_pool_spare(&inst.name);
                        }
                    }
                    Err(e) => thegn_core::msg::warn(&format!(
                        "vps reaper: destroy {} failed: {e}; will retry next pass",
                        inst.name
                    )),
                }
            }
        }
        let _ = env_name; // env identity only matters for per-env lifetime caps above
    }

    // Ledger cleanup runs ONCE, after every account is listed, against the
    // union of live names — so a record whose live instance lives under a
    // sibling account (same kind, different api_base) is never dropped. Only
    // kinds where every account listed cleanly are eligible.
    for k in &failed_kinds {
        listed_kinds.remove(k);
    }
    let records: Vec<registry::VpsRecord> = registry::list()
        .into_iter()
        .filter(|r| listed_kinds.contains(&r.provider))
        .collect();
    cleanup_ledger(&records, &live_anywhere);
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
    let stale_creating = rec.state == "creating" && now - rec.created_at >= CREATING_STALE_SECS;
    let gone_ready = rec.state == "ready";
    stale_creating || gone_ready
}

/// Drop ledger records with no live instance ANYWHERE (across all accounts of
/// the same kind): stale `creating` (the POST never landed) or a `ready` record
/// whose instance is gone out-of-band.
fn cleanup_ledger(records: &[registry::VpsRecord], live: &std::collections::HashSet<String>) {
    let now = thegn_core::util::now();
    for rec in records {
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
        live.insert("sz-work-live".to_string()); // seen live under account 2
        let r = rec("sz-work-live", "ready", now - 10_000);
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
        assert!(should_drop_record(&rec("gone", "ready", now - 10_000), &live, now));
        // creating older than the stale threshold ⇒ drop.
        assert!(should_drop_record(
            &rec("stuck", "creating", now - CREATING_STALE_SECS - 1),
            &live,
            now
        ));
        // creating but still fresh ⇒ keep (create may be in flight).
        assert!(!should_drop_record(&rec("fresh", "creating", now - 1), &live, now));
    }
}
