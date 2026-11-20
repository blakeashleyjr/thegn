//! Vercel lifetime reaper, on the hydration cadence beside `fly_reaper`: at
//! `max_lifetime_secs` a sandbox is STOPPED (compute to $0) and its ledger
//! record marked expired; it is deleted (with its snapshots) only after the
//! grace period, or at once when its worktree is gone. Never creates or
//! resumes. Inert with no vercel env (an early-return config scan).
//!
//! - Every remote read is `VercelProvider::reaper_view` (a GET that never
//!   resumes), so a pass never buys compute.
//! - A `creating` record older than [`CREATING_STALE_SECS`] is a crashed create:
//!   best-effort delete of whatever landed, then drop the record.
//! - Records stamped for another provider account (or whose account cannot be
//!   computed) are quarantined before any provider call.
//! - Unknown provider age never authorizes an action.
//! - Every failure keeps the ledger record; "done" means the remote is stopped
//!   or deleted (THE-2019).
//! - Orphans (listed by Vercel, absent from the ledger) are reported in the
//!   [`KeptReport`](crate::reaper_policy::KeptReport) and never deleted here.
//!
//! No subprocess anywhere: the soak exec ceiling for this path is 0.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use thegn_core::config::{Config, EnvProviderConfig};
use thegn_core::vercel::api::SandboxView;
use thegn_core::vercel::policy::{self, ReapAction};
use thegn_svc::vps::registry;

const KIND: &str = "vercel";
const TICK_INTERVAL: Duration = Duration::from_secs(300);
/// Mirrors the fly/VPS reapers' stale-`creating` threshold.
const CREATING_STALE_SECS: u64 = 10 * 60;
/// The ledger state a record gets once its sandbox is stopped at the ceiling.
const STATE_EXPIRED: &str = "expired";

fn is_vercel_env(pc: &EnvProviderConfig) -> bool {
    pc.provider.trim() == KIND
}

/// Throttled entry: schedule one reconcile pass when due. Cheap (and free) when
/// no `provider = "vercel"` env is configured or the ledger has no Vercel work.
pub fn tick(cfg: &Config) {
    static LEDGER_WARNED: AtomicBool = AtomicBool::new(false);
    let envs: Vec<EnvProviderConfig> = cfg
        .env
        .values()
        .filter(|e| is_vercel_env(&e.provider))
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
                    "vercel reaper: skipped vercel: ledger unreadable ({error:#}); no provider or ledger actions attempted"
                ));
            }
            return;
        }
    };
    if !has_vercel_work(&view) {
        return;
    }
    static GATE: crate::reaper_policy::ReaperGate = crate::reaper_policy::ReaperGate::new();
    let Some(token) = GATE.try_begin(TICK_INTERVAL) else {
        if let Some(elapsed) =
            GATE.stalled_for(Instant::now(), crate::reaper_policy::STALL_WARN_AFTER)
        {
            let minutes = elapsed.as_secs() / 60;
            tracing::warn!(target: "thegn::lifecycle", "vercel reaper pass has run for {minutes} min; a provider call may be hung; no new pass starts until it ends");
        }
        return;
    };
    if !crate::reaper_policy::spawn_pass(token, "thegn-vx-reap", move || {
        reap(&envs);
    }) {
        tracing::warn!(target: "thegn::lifecycle", "vercel reaper thread could not start");
    }
}

fn reap(envs: &[EnvProviderConfig]) {
    let mut kept = crate::reaper_policy::KeptReport::default();
    let lease = match crate::reaper_policy::try_reaper_lease(KIND) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            tracing::debug!(target: "thegn::lifecycle", "another thegn process is reconciling vercel");
            return;
        }
        Err(reason) => {
            kept.keep(
                KIND,
                format!(
                    "vercel reaper: skipped vercel: {reason}; no provider or ledger actions attempted"
                ),
            );
            kept.publish(KIND);
            return;
        }
    };
    let _lease = lease;
    if let Err(reason) =
        crate::reaper_policy::with_unambiguous(envs, |pc| reap_unambiguous(pc, &mut kept))
    {
        kept.keep(KIND, format!(
            "vercel reaper: quarantined provider kind: {reason}; no provider or ledger actions attempted"
        ));
    }
    kept.publish(KIND);
}

fn reap_unambiguous(pc: &EnvProviderConfig, kept: &mut crate::reaper_policy::KeptReport) {
    let view = match registry::list_checked() {
        Ok(view) => view,
        Err(error) => {
            kept.keep(KIND, format!(
                "vercel reaper: skipped vercel: ledger unreadable ({error:#}); no provider or ledger actions attempted"
            ));
            return;
        }
    };
    for name in &view.unreadable {
        kept.keep(name, format!(
            "vercel reaper: ledger record {name} unreadable (provider unknown); reconcile ownership; no deletion attempted"
        ));
    }
    let lifetime = policy::max_lifetime_secs(pc.max_lifetime_secs);
    let records: Vec<registry::VpsRecord> = view
        .records
        .iter()
        .filter(|r| r.provider == KIND)
        .cloned()
        .collect();
    let Some(provider) = crate::provider_factory::vercel_provider_for(pc, "") else {
        for rec in &records {
            kept.keep(&rec.name, format!(
                "vercel reaper: quarantined {}: credentials or scope unavailable; no deletion attempted",
                rec.name
            ));
        }
        return;
    };
    let account = provider.account();
    let controller_tag = match thegn_svc::vps::controller::controller_id() {
        Ok(id) => policy::tags_for(&id, "", "")
            .remove(policy::CONTROLLER_TAG)
            .unwrap_or_default(),
        Err(error) => {
            for rec in &records {
                kept.keep(&rec.name, format!(
                    "vercel reaper: quarantined {}: controller id unavailable ({error:#}); no deletion attempted",
                    rec.name
                ));
            }
            return;
        }
    };
    let mut owners = LazyOwners::default();
    let live = LiveVercel {
        provider: &provider,
    };
    for rec in &records {
        if lifetime > thegn_core::time_policy::MAX_DURATION_SECS {
            kept.keep(&rec.name, format!(
                "vercel reaper: quarantined {}: unsupported lifetime policy; repair duration configuration; no deletion attempted",
                rec.name
            ));
            continue;
        }
        if let Some(reason) = account_quarantine(rec, &account) {
            kept.keep(
                &rec.name,
                format!(
                    "vercel reaper: quarantined {}: {reason}; no deletion attempted",
                    rec.name
                ),
            );
            continue;
        }
        let owner_alive = owners.alive(&rec.owner);
        let mut ledger = LiveLedger { pc };
        let outcome = reconcile_record(
            rec,
            thegn_core::util::now(),
            lifetime,
            owner_alive,
            &live,
            &mut ledger,
            &controller_tag,
        );
        match outcome {
            Outcome::Idle => {}
            Outcome::Stopped => thegn_core::msg::info(&format!(
                "vercel reaper: stopped {} at its lifetime ceiling (compute now $0; `thegn env pull` before it is deleted)",
                rec.name
            )),
            Outcome::Deleted => thegn_core::msg::info(&format!(
                "vercel reaper: deleted {} and its snapshots",
                rec.name
            )),
            Outcome::Dropped => thegn_core::msg::info(&format!(
                "vercel reaper: dropped stale creating record {}",
                rec.name
            )),
            Outcome::Kept(message) => kept.keep(&rec.name, message),
        }
    }
    // Orphans: listed by Vercel (lane 4's inventory is scoped to this
    // controller's tagged sandboxes) but absent from the ledger. Report only.
    let known: HashSet<&str> = view
        .records
        .iter()
        .map(|r| r.name.as_str())
        .chain(view.unreadable.iter().map(String::as_str))
        .collect();
    match crate::agent::block_on_provider(|| async { provider.sandbox_inventory().await }) {
        Ok(report) => {
            let rows =
                match report {
                    thegn_svc::provider::InventoryReport::Complete(rows) => rows,
                    thegn_svc::provider::InventoryReport::Partial { rows, reason } => {
                        kept.keep(KIND, format!(
                        "vercel reaper: inventory partial ({reason}); orphan check incomplete"
                    ));
                        rows
                    }
                };
            let ids: Vec<String> = rows.into_iter().map(|r| r.id).collect();
            let invalid = ids
                .iter()
                .filter(|id| !thegn_core::vercel::admit::valid_sandbox_name(id))
                .count();
            if invalid > 0 {
                kept.keep(
                    KIND,
                    format!(
                        "vercel reaper: {invalid} listed sandbox name(s) failed validation; not shown"
                    ),
                );
            }
            for name in orphans(&ids, &known) {
                kept.keep(&name, orphan_message(&name));
            }
        }
        Err(error) => kept.keep(
            KIND,
            format!("vercel reaper: inventory unavailable ({error:#}); orphan check skipped"),
        ),
    }
}

fn orphan_message(name: &str) -> String {
    format!(
        "vercel reaper: orphan {name}: a Vercel sandbox tagged for this controller has no ledger record; not deleted automatically; reconcile it by hand"
    )
}

/// Inventory ids absent from the ledger, sorted and deduplicated.
fn orphans(ids: &[String], known: &HashSet<&str>) -> Vec<String> {
    let mut out: Vec<String> = ids
        .iter()
        .filter(|id| thegn_core::vercel::admit::valid_sandbox_name(id))
        .filter(|id| !known.contains(id.as_str()))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The set of owner digests of registered thegn worktrees, read once per pass
/// from the state DB (read-only). `None` when the DB cannot be read: an
/// unknown owner is treated as alive, so it never authorizes a delete.
#[derive(Default)]
struct LazyOwners {
    loaded: Option<Option<HashSet<String>>>,
}

impl LazyOwners {
    fn alive(&mut self, owner: &str) -> bool {
        let set = self.loaded.get_or_insert_with(registered_owners);
        owner_alive(owner, set.as_ref())
    }
}

fn registered_owners() -> Option<HashSet<String>> {
    use thegn_core::store::WorkspaceStore;
    let db = thegn_core::db::Db::open_read_only_current().ok()??;
    let rows = db.worktrees().ok()?;
    Some(
        rows.iter()
            .filter(|w| !w.worktree.is_empty())
            .map(|w| registry::worktree_owner(w.worktree.as_bytes()))
            .collect(),
    )
}

/// Whether the ledger owner still exists as a thegn worktree. An empty
/// (legacy/claimed) owner or an unreadable worktree set is "alive": only a
/// positive absence may authorize the owner-gone delete.
fn owner_alive(owner: &str, registered: Option<&HashSet<String>>) -> bool {
    if owner.is_empty() {
        return true;
    }
    registered
        .filter(|set| !set.is_empty())
        .is_none_or(|set| set.contains(owner))
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

fn has_vercel_work(view: &registry::LedgerView) -> bool {
    view.records.iter().any(|record| record.provider == KIND) || !view.unreadable.is_empty()
}

fn should_warn_ledger_error(warned: &AtomicBool) -> bool {
    !warned.swap(true, Ordering::AcqRel)
}

fn reset_ledger_warning(warned: &AtomicBool) {
    warned.store(false, Ordering::Release);
}

/// The remote half one record's reconcile needs. Never creates or resumes.
trait VercelRemote {
    fn view(&self, name: &str) -> Result<Option<SandboxView>>;
    fn stop(&self, name: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

/// The local half: the ledger fence, the expired mark, route-token revocation
/// and retirement.
trait VercelLedger {
    fn unchanged(&self, rec: &registry::VpsRecord) -> bool;
    fn mark_expired(&mut self, rec: &registry::VpsRecord) -> Result<()>;
    fn revoke_route_token(&mut self, name: &str) -> Result<()>;
    fn retire(&mut self, name: &str) -> Result<()>;
}

struct LiveVercel<'a> {
    provider: &'a thegn_svc::vercel::VercelProvider,
}

impl VercelRemote for LiveVercel<'_> {
    fn view(&self, name: &str) -> Result<Option<SandboxView>> {
        crate::agent::block_on_provider(|| async { self.provider.reaper_view(name).await })
    }
    fn stop(&self, name: &str) -> Result<()> {
        crate::agent::block_on_provider(|| async { self.provider.stop_session(name).await })
    }
    fn delete(&self, name: &str) -> Result<()> {
        crate::agent::block_on_provider(|| async {
            self.provider.delete_with_snapshots(name).await
        })
    }
}

struct LiveLedger<'a> {
    pc: &'a EnvProviderConfig,
}

impl VercelLedger for LiveLedger<'_> {
    fn unchanged(&self, rec: &registry::VpsRecord) -> bool {
        registry::list_checked()
            .map(|fresh| record_unchanged(rec, &fresh))
            .unwrap_or(false)
    }
    fn mark_expired(&mut self, rec: &registry::VpsRecord) -> Result<()> {
        let mut expired = rec.clone();
        expired.state = STATE_EXPIRED.to_string();
        registry::write(&expired)
    }
    fn revoke_route_token(&mut self, name: &str) -> Result<()> {
        crate::remote_enqueue_auth::revoke_for_sandbox(self.pc, name, None).map(|_| ())
    }
    fn retire(&mut self, name: &str) -> Result<()> {
        registry::remove(name);
        match registry::read_checked(name) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => anyhow::bail!("the ledger record is still present after removal"),
            Err(error) => Err(error.context("ledger re-read after removal failed")),
        }
    }
}

/// Whether the record a pass snapshotted is still exactly what the ledger holds.
fn record_unchanged(snapshot: &registry::VpsRecord, fresh: &registry::LedgerView) -> bool {
    if fresh.unreadable.iter().any(|n| n == &snapshot.name) {
        return false;
    }
    fresh
        .records
        .iter()
        .any(|r| r.name == snapshot.name && r == snapshot)
}

/// What one record's reconcile did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// Nothing due.
    Idle,
    /// Session stopped at the ceiling; the record is now `expired`.
    Stopped,
    /// Sandbox and snapshots deleted; the record is retired.
    Deleted,
    /// A stale `creating` record whose sandbox is confirmed absent/deleted.
    Dropped,
    /// Left in place (quarantine, refusal or failure), with the reason.
    Kept(String),
}

/// One record: decide from the never-resuming remote view, then act in a
/// fail-closed order. Every failure keeps the record (and reports why).
fn reconcile_record(
    rec: &registry::VpsRecord,
    now_secs: i64,
    lifetime_cap_secs: u64,
    owner_alive: bool,
    remote: &impl VercelRemote,
    ledger: &mut impl VercelLedger,
    controller_tag: &str,
) -> Outcome {
    let name = rec.name.as_str();
    let kept = |why: String| Outcome::Kept(format!("vercel reaper: kept {name}: {why}"));
    if rec.state == "creating" {
        return reconcile_creating(rec, now_secs, remote, ledger, controller_tag);
    }
    let view = match remote.view(name) {
        Ok(Some(view)) => view,
        Ok(None) => {
            return kept(
                "Vercel reports no such sandbox; the record is kept until ownership is reconciled; no action attempted"
                    .into(),
            );
        }
        Err(error) => {
            return kept(format!(
                "status unavailable ({error:#}); no action attempted; next pass will retry"
            ));
        }
    };
    let now_ms = now_secs.saturating_mul(1000);
    // Unknown provider age (missing, or in the future) never authorizes an
    // action, whatever the policy would say.
    let Some(created_ms) = view.created_at_ms.filter(|&c| c > 0 && c <= now_ms) else {
        return kept("provider creation time unknown; no action attempted".into());
    };
    match policy::reap_action(
        now_ms,
        Some(created_ms),
        view.status,
        owner_alive,
        lifetime_cap_secs,
    ) {
        ReapAction::Keep => Outcome::Idle,
        ReapAction::Quarantine(reason) => kept(format!("{reason}; no action attempted")),
        ReapAction::Stop => {
            if !proven_ours(&view, rec, controller_tag) {
                return kept(NOT_OURS.into());
            }
            if !ledger.unchanged(rec) {
                return kept(
                    "the ledger record changed during this pass; next pass will retry; no action attempted"
                        .into(),
                );
            }
            if let Err(error) = remote.stop(name) {
                return kept(format!(
                    "stop at the lifetime ceiling failed ({error:#}); the record remains for the next pass"
                ));
            }
            if rec.state == STATE_EXPIRED {
                return Outcome::Stopped;
            }
            match ledger.mark_expired(rec) {
                Ok(()) => Outcome::Stopped,
                Err(error) => kept(format!(
                    "stopped, but marking the ledger record expired failed ({error:#}); next pass will retry"
                )),
            }
        }
        ReapAction::Delete => {
            if !proven_ours(&view, rec, controller_tag) {
                return kept(NOT_OURS.into());
            }
            if !ledger.unchanged(rec) {
                return kept(
                    "the ledger record changed during this pass; next pass will retry; no deletion attempted"
                        .into(),
                );
            }
            match delete_then_retire(name, remote, ledger) {
                Ok(()) => Outcome::Deleted,
                Err(error) => kept(format!(
                    "delete failed: {error:#}; ownership records remain for the next pass"
                )),
            }
        }
    }
}

/// A crashed create (`creating` past [`CREATING_STALE_SECS`] by the local
/// intent clock): delete whatever landed, then drop the record. A sandbox that
/// is confirmed absent is dropped without a delete.
fn reconcile_creating(
    rec: &registry::VpsRecord,
    now_secs: i64,
    remote: &impl VercelRemote,
    ledger: &mut impl VercelLedger,
    controller_tag: &str,
) -> Outcome {
    let name = rec.name.as_str();
    let kept = |why: String| Outcome::Kept(format!("vercel reaper: kept {name}: {why}"));
    if rec.created_at <= 0 || rec.created_at > now_secs {
        return kept("creating record has an unknown intent time; no action attempted".into());
    }
    let age = u64::try_from(now_secs - rec.created_at).unwrap_or(0);
    if age < CREATING_STALE_SECS {
        return Outcome::Idle;
    }
    if !ledger.unchanged(rec) {
        return kept(
            "the ledger record changed during this pass; next pass will retry; no deletion attempted"
                .into(),
        );
    }
    let present = match remote.view(name) {
        Ok(Some(v)) => {
            let landed_after_intent = v
                .created_at_ms
                .is_some_and(|c| c >= rec.created_at.saturating_sub(300).saturating_mul(1000));
            if !proven_ours(&v, rec, controller_tag) || !landed_after_intent {
                return kept(NOT_OURS.into());
            }
            true
        }
        Ok(None) => false,
        Err(error) => {
            return kept(format!(
                "stale create status unavailable ({error:#}); next pass will retry"
            ));
        }
    };
    let result = if present {
        delete_then_retire(name, remote, ledger)
    } else {
        ledger
            .revoke_route_token(name)
            .context("route-to-host credential revocation failed")
            .and_then(|()| {
                ledger
                    .retire(name)
                    .context("local ownership retirement failed")
            })
    };
    match result {
        Ok(()) => Outcome::Dropped,
        Err(error) => kept(format!(
            "stale create cleanup failed: {error:#}; ownership records remain for the next pass"
        )),
    }
}

/// The remote sandbox carries this controller's tag (and the ledger owner's
/// tag when one is recorded): the proof thegn created it.
/// The check is the shared [`policy::tags_prove_ours`], the same one the
/// provider's delete reads, over the tags its create wrote.
fn proven_ours(view: &SandboxView, rec: &registry::VpsRecord, controller_tag: &str) -> bool {
    policy::tags_prove_ours(&view.tags, controller_tag, &rec.owner)
}

const NOT_OURS: &str = "the Vercel sandbox does not carry this controller's ownership tags; not provably created by thegn; no action attempted";

/// Fail-closed order: route-token revocation, remote delete (sandbox and its
/// snapshots), then local retirement. Only a successful remote delete may
/// retire the record.
fn delete_then_retire(
    name: &str,
    remote: &impl VercelRemote,
    ledger: &mut impl VercelLedger,
) -> Result<()> {
    ledger
        .revoke_route_token(name)
        .context("route-to-host credential revocation failed")?;
    remote.delete(name).context("remote delete failed")?;
    ledger
        .retire(name)
        .context("local ownership retirement failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use thegn_core::vercel::api::SessionStatus;

    const NOW: i64 = 1_000_000;

    fn record(state: &str, account: &str) -> registry::VpsRecord {
        registry::VpsRecord {
            name: "tg-vx".to_string(),
            provider: "vercel".to_string(),
            state: state.to_string(),
            instance_id: String::new(),
            ip: String::new(),
            created_at: NOW - 3600,
            owner: String::new(),
            account: account.to_string(),
        }
    }

    fn sandbox(status: SessionStatus, created_at_ms: Option<i64>) -> SandboxView {
        SandboxView {
            name: "tg-vx".into(),
            status,
            session_id: None,
            vcpus: Some(2),
            region: None,
            created_at_ms,
            tags: BTreeMap::from([("thegn-controller".to_string(), "ctl-1".to_string())]),
            cwd: None,
        }
    }

    /// A scripted remote + ledger that records every call in order.
    struct Fake {
        view: RefCell<Option<Result<Option<SandboxView>>>>,
        fail: Option<&'static str>,
        unchanged: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(view: Result<Option<SandboxView>>) -> Self {
            Fake {
                view: RefCell::new(Some(view)),
                fail: None,
                unchanged: true,
                calls: RefCell::new(Vec::new()),
            }
        }
        fn step(&self, call: &'static str) -> Result<()> {
            self.calls.borrow_mut().push(call);
            anyhow::ensure!(self.fail != Some(call), "injected {call} failure");
            Ok(())
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }
    }

    impl VercelRemote for Fake {
        fn view(&self, _name: &str) -> Result<Option<SandboxView>> {
            self.calls.borrow_mut().push("view");
            self.view
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("view called twice")))
        }
        fn stop(&self, _name: &str) -> Result<()> {
            self.step("stop")
        }
        fn delete(&self, _name: &str) -> Result<()> {
            self.step("delete")
        }
    }

    impl VercelLedger for &Fake {
        fn unchanged(&self, _rec: &registry::VpsRecord) -> bool {
            self.unchanged
        }
        fn mark_expired(&mut self, _rec: &registry::VpsRecord) -> Result<()> {
            self.step("mark_expired")
        }
        fn revoke_route_token(&mut self, _name: &str) -> Result<()> {
            self.step("revoke")
        }
        fn retire(&mut self, _name: &str) -> Result<()> {
            self.step("retire")
        }
    }

    fn run(rec: &registry::VpsRecord, fake: &Fake, owner_alive: bool) -> Outcome {
        let mut ledger = fake;
        reconcile_record(rec, NOW, 600, owner_alive, fake, &mut ledger, "ctl-1")
    }

    #[test]
    fn tags_written_at_create_are_proven_ours_by_the_reaper() {
        // Exactly what the provider's create sends: `tags_for(controller id,
        // ledger owner, host label)`; the reaper derives its tag the same way.
        let raw_controller = "Ctl_0123ABCD";
        let owner = "wt-owner-digest";
        let mut rec = record("ready", "");
        rec.owner = owner.to_string();
        let mut view = sandbox(SessionStatus::Running, Some(1));
        view.tags = policy::tags_for(raw_controller, owner, "some host");
        let reaper_tag = policy::tags_for(raw_controller, "", "")
            .remove(policy::CONTROLLER_TAG)
            .unwrap_or_default();
        assert!(proven_ours(&view, &rec, &reaper_tag));
        rec.owner = "another-owner".to_string();
        assert!(!proven_ours(&view, &rec, &reaper_tag));
        rec.owner = String::new();
        assert!(!proven_ours(&view, &rec, "other-controller"));
        assert!(!proven_ours(&view, &rec, ""));
    }

    #[test]
    fn account_gate_lets_legacy_and_same_account_records_proceed() {
        assert_eq!(
            account_quarantine(&record("ready", ""), &Ok("acct1-a".into())),
            None
        );
        assert_eq!(
            account_quarantine(&record("ready", "acct1-a"), &Ok("acct1-a".into())),
            None
        );
    }

    #[test]
    fn account_gate_quarantines_another_accounts_record() {
        let reason = account_quarantine(&record("ready", "acct1-a"), &Ok("acct1-b".into()))
            .expect("different accounts must be quarantined");
        assert!(reason.contains("another provider account"));
    }

    #[test]
    fn account_gate_quarantines_when_the_account_is_unavailable() {
        let reason = account_quarantine(
            &record("ready", ""),
            &Err(anyhow::anyhow!("authority unavailable")),
        )
        .expect("unavailable account must be quarantined");
        assert!(reason.contains("provider account unavailable"));
        assert!(reason.contains("authority unavailable"));
    }

    #[test]
    fn unknown_age_never_reaches_any_lifecycle_action() {
        let now_ms = NOW * 1000;
        for created in [None, Some(0), Some(-5), Some(now_ms + 1)] {
            for alive in [true, false] {
                let fake = Fake::new(Ok(Some(sandbox(SessionStatus::Running, created))));
                let outcome = run(&record("ready", ""), &fake, alive);
                assert!(
                    matches!(&outcome, Outcome::Kept(m) if m.contains("creation time unknown")),
                    "{created:?}: {outcome:?}"
                );
                assert_eq!(fake.calls(), ["view"], "{created:?} must only read");
            }
        }
    }

    #[test]
    fn unknown_intent_time_on_a_creating_record_never_acts() {
        for created_at in [0, -1, NOW + 1] {
            let mut rec = record("creating", "");
            rec.created_at = created_at;
            let fake = Fake::new(Ok(Some(sandbox(SessionStatus::Running, Some(1)))));
            assert!(matches!(run(&rec, &fake, false), Outcome::Kept(_)));
            assert!(fake.calls().is_empty());
        }
    }

    #[test]
    fn a_status_read_failure_or_missing_sandbox_keeps_the_record() {
        let fake = Fake::new(Err(anyhow::anyhow!("503")));
        assert!(
            matches!(run(&record("ready", ""), &fake, false), Outcome::Kept(m) if m.contains("status unavailable"))
        );
        assert_eq!(fake.calls(), ["view"]);

        let fake = Fake::new(Ok(None));
        assert!(matches!(
            run(&record("ready", ""), &fake, false),
            Outcome::Kept(_)
        ));
        assert_eq!(fake.calls(), ["view"]);
    }

    #[test]
    fn a_fresh_creating_record_is_left_alone_without_any_remote_call() {
        let mut rec = record("creating", "");
        rec.created_at = NOW - 60;
        let fake = Fake::new(Ok(None));
        assert_eq!(run(&rec, &fake, true), Outcome::Idle);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_stale_creating_record_is_deleted_then_dropped() {
        let mut rec = record("creating", "");
        rec.created_at = NOW - CREATING_STALE_SECS as i64 - 1;
        let fake = Fake::new(Ok(Some(sandbox(
            SessionStatus::Pending,
            Some((NOW - CREATING_STALE_SECS as i64 - 1) * 1000),
        ))));
        assert_eq!(run(&rec, &fake, true), Outcome::Dropped);
        assert_eq!(fake.calls(), ["view", "revoke", "delete", "retire"]);

        // A create that never landed: nothing to delete, just drop.
        let fake = Fake::new(Ok(None));
        assert_eq!(run(&rec, &fake, true), Outcome::Dropped);
        assert_eq!(fake.calls(), ["view", "revoke", "retire"]);
    }

    #[test]
    fn retire_only_after_remote_delete_succeeds() {
        let ok = Fake::new(Ok(None));
        let mut ledger = &ok;
        delete_then_retire("tg-vx", &ok, &mut ledger).unwrap();
        assert_eq!(ok.calls(), ["revoke", "delete", "retire"]);
    }

    #[test]
    fn every_failure_keeps_the_record() {
        for (failure, expected) in [
            ("revoke", vec!["revoke"]),
            ("delete", vec!["revoke", "delete"]),
            ("retire", vec!["revoke", "delete", "retire"]),
        ] {
            let mut fake = Fake::new(Ok(None));
            fake.fail = Some(failure);
            let mut ledger = &fake;
            let result = delete_then_retire("tg-vx", &fake, &mut ledger);
            assert!(result.is_err(), "{failure} must surface");
            assert_eq!(
                fake.calls(),
                expected,
                "unexpected lifecycle after {failure}"
            );
        }

        // A stale create whose delete fails is kept, never dropped.
        let mut rec = record("creating", "");
        rec.created_at = 1;
        let mut fake = Fake::new(Ok(Some(sandbox(SessionStatus::Running, Some(1000)))));
        fake.fail = Some("delete");
        assert!(matches!(run(&rec, &fake, true), Outcome::Kept(_)));
        assert!(!fake.calls().contains(&"retire"));

        // A changed ledger record blocks every destructive step.
        let mut fake = Fake::new(Ok(None));
        fake.unchanged = false;
        assert!(matches!(run(&rec, &fake, true), Outcome::Kept(m) if m.contains("changed")));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn checked_ledger_unreadable_entry_is_work_and_an_empty_ledger_is_not() {
        let empty = registry::LedgerView {
            records: Vec::new(),
            unreadable: Vec::new(),
        };
        assert!(!has_vercel_work(&empty));
        let mut fly = record("ready", "");
        fly.provider = "fly".into();
        let other = registry::LedgerView {
            records: vec![fly],
            unreadable: Vec::new(),
        };
        assert!(!has_vercel_work(&other));
        let broken = registry::LedgerView {
            records: Vec::new(),
            unreadable: vec!["broken".into()],
        };
        assert!(has_vercel_work(&broken));
        let ours = registry::LedgerView {
            records: vec![record("ready", "")],
            unreadable: Vec::new(),
        };
        assert!(has_vercel_work(&ours));
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
    fn tick_without_a_vercel_env_is_inert() {
        // No env at all, and a fly-only env: early return before any I/O.
        let mut cfg = Config::default();
        tick(&cfg);
        cfg.env.insert(
            "f".into(),
            thegn_core::config::EnvConfig {
                provider: EnvProviderConfig {
                    provider: "fly".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        tick(&cfg);
        assert!(!is_vercel_env(&cfg.env["f"].provider));
        assert!(is_vercel_env(&EnvProviderConfig {
            provider: " vercel ".into(),
            ..Default::default()
        }));
    }

    #[test]
    fn owner_alive_needs_a_positive_absence_to_be_gone() {
        let set: HashSet<String> = [registry::worktree_owner(b"/w/a")].into_iter().collect();
        let a = registry::worktree_owner(b"/w/a");
        let b = registry::worktree_owner(b"/w/b");
        assert!(owner_alive(&a, Some(&set)));
        assert!(!owner_alive(&b, Some(&set)));
        // Legacy (empty) owner and an unreadable worktree set are alive.
        assert!(owner_alive("", Some(&set)));
        assert!(owner_alive(&b, None));
        assert!(owner_alive(&b, Some(&HashSet::new())));
    }

    #[test]
    fn record_unchanged_matrix() {
        let snap = record("ready", "acct");
        let view =
            |records: Vec<registry::VpsRecord>, unreadable: Vec<String>| registry::LedgerView {
                records,
                unreadable,
            };
        assert!(record_unchanged(&snap, &view(vec![snap.clone()], vec![])));
        assert!(!record_unchanged(&snap, &view(vec![], vec![])));
        let mut expired = snap.clone();
        expired.state = STATE_EXPIRED.into();
        assert!(!record_unchanged(&snap, &view(vec![expired], vec![])));
        assert!(!record_unchanged(
            &snap,
            &view(vec![snap.clone()], vec![snap.name.clone()])
        ));
    }

    #[test]
    fn orphans_are_listed_sorted_and_never_include_ledger_names() {
        let known: HashSet<&str> = ["tg-a", "broken"].into_iter().collect();
        let ids = vec![
            "tg-c".to_string(),
            "tg-a".to_string(),
            "tg-b".to_string(),
            "tg-c".to_string(),
            "broken".to_string(),
            "Tg-\u{1b}[2J".to_string(),
        ];
        assert_eq!(orphans(&ids, &known), ["tg-b", "tg-c"]);
        assert!(orphan_message("tg-b").contains("not deleted automatically"));
    }

    fn ms(age: i64) -> Option<i64> {
        Some((NOW - age) * 1000)
    }

    #[test]
    fn look_alike_sandboxes_are_never_stopped_or_deleted() {
        for alive in [true, false] {
            let mut view = sandbox(SessionStatus::Running, Some((NOW - 700) * 1000));
            view.tags
                .insert("thegn-controller".to_string(), "other".to_string());
            let fake = Fake::new(Ok(Some(view)));
            let outcome = run(&record("ready", ""), &fake, alive);
            assert!(matches!(outcome, Outcome::Kept(_)), "{alive}: {outcome:?}");
            assert_eq!(fake.calls(), ["view"], "owner_alive={alive}");
        }

        let mut rec = record("creating", "");
        rec.created_at = NOW - CREATING_STALE_SECS as i64 - 1;
        let mut view = sandbox(SessionStatus::Running, Some(rec.created_at * 1000));
        view.tags
            .insert("thegn-controller".to_string(), "other".to_string());
        let fake = Fake::new(Ok(Some(view)));
        assert!(matches!(run(&rec, &fake, true), Outcome::Kept(_)));
        assert_eq!(fake.calls(), ["view"]);

        let fake = Fake::new(Ok(Some(sandbox(
            SessionStatus::Running,
            Some((rec.created_at - 301) * 1000),
        ))));
        assert!(matches!(run(&rec, &fake, true), Outcome::Kept(_)));
        assert_eq!(fake.calls(), ["view"]);
    }

    #[test]
    fn running_sandbox_at_the_ceiling_is_stopped_and_marked_expired() {
        let fake = Fake::new(Ok(Some(sandbox(SessionStatus::Running, ms(600)))));
        assert_eq!(run(&record("ready", ""), &fake, true), Outcome::Stopped);
        assert_eq!(fake.calls(), ["view", "stop", "mark_expired"]);
    }

    #[test]
    fn an_expired_record_is_stopped_without_remarking() {
        let fake = Fake::new(Ok(Some(sandbox(SessionStatus::Running, ms(600)))));
        assert_eq!(
            run(&record(STATE_EXPIRED, ""), &fake, true),
            Outcome::Stopped
        );
        assert_eq!(fake.calls(), ["view", "stop"]);
    }

    #[test]
    fn a_failed_stop_keeps_the_record_unmarked() {
        let mut fake = Fake::new(Ok(Some(sandbox(SessionStatus::Running, ms(600)))));
        fake.fail = Some("stop");
        assert!(matches!(
            run(&record("ready", ""), &fake, true),
            Outcome::Kept(_)
        ));
        assert_eq!(fake.calls(), ["view", "stop"]);
    }

    #[test]
    fn a_stopped_sandbox_is_deleted_only_after_the_grace_period() {
        use thegn_core::vercel::policy::DELETE_GRACE_SECS;
        let fake = Fake::new(Ok(Some(sandbox(
            SessionStatus::Stopped,
            ms(600 + DELETE_GRACE_SECS as i64 - 1),
        ))));
        assert_eq!(run(&record("ready", ""), &fake, true), Outcome::Idle);
        assert_eq!(fake.calls(), ["view"]);

        let fake = Fake::new(Ok(Some(sandbox(
            SessionStatus::Stopped,
            ms(600 + DELETE_GRACE_SECS as i64),
        ))));
        assert_eq!(run(&record("ready", ""), &fake, true), Outcome::Deleted);
        assert_eq!(fake.calls(), ["view", "revoke", "delete", "retire"]);
    }

    #[test]
    fn a_sandbox_whose_owner_is_gone_is_deleted() {
        let fake = Fake::new(Ok(Some(sandbox(SessionStatus::Running, ms(10)))));
        assert_eq!(run(&record("ready", ""), &fake, false), Outcome::Deleted);
        assert_eq!(fake.calls(), ["view", "revoke", "delete", "retire"]);
    }
}
