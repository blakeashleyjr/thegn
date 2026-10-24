//! The placement engine's impure flow: snapshot the host pool, run the pure
//! broker ([`thegn_core::scheduler::decide_placement`]), atomically reserve
//! the chosen slot, and hand the winning [`HostBinding`] to the unchanged
//! `host_flow::ensure_ready`. All blocking (DB + config resolution) — runs in
//! the same spawn_blocking provisioning contexts as everything it fronts.
//!
//! Engine scope (deliberately narrow): envs with an explicit `host = "..."`
//! pin bypass the broker (recorded as `pinned` tenancy so capacity accounting
//! stays truthful); inline-ssh / provider / k8s envs keep their own targets;
//! only local-placement (or implicit) envs are engine-placed, and only while
//! `[placement] enabled = true`. Engine off ⇒ every path is byte-identical.
//!
//! The tenancy key for a worktree's engine placement is the WORKTREE PATH
//! (unique, stable, restart-safe); warm-pool spares key by their minted
//! sandbox name — the two namespaces never collide (paths contain `/`).

use std::collections::BTreeSet;

use thegn_core::capacity::{HostCapacity, HostOwnership, HostSpec, MeasuredLoad};
use thegn_core::config::{Config, EnvConfig, PlacementMode};
use thegn_core::config_placement::{OnExhaustion, ResolvedPlacement, resolve_placement};
use thegn_core::db::Db;
use thegn_core::db_compute::{ComputeVerdict, check_compute_budget};
use thegn_core::host::{HostFailure, HostId, HostStep};
use thegn_core::host_config::{HostBinding, HostConfig, HostReach};
use thegn_core::host_machine::HostState;
use thegn_core::scheduler::{
    AutoscaleSnapshot, DecisionOutput, HostSnapshot, PlacementDecision, PlacementInputs,
    PlacementRequest, decide_placement,
};
use thegn_core::store::{
    HealthMarker, HostStore, PlacementEventRow, PlacementStore, ReserveOutcome, TenancyMode,
    TenancyRow, TenancyState, WorkspaceStore, ZoneStore,
};
use thegn_core::trust_class::{TrustClass, effective_class, required_class};

/// Reservations older than this in `reserved` (never activated) are released
/// by the maintainer sweep — a crashed driver must not hold capacity forever.
const RESERVATION_TTL_SECS: i64 = 30 * 60;
/// Bounded re-decide rounds when reservation races are lost.
const RESERVE_ROUNDS: usize = 3;

/// Whether the caller may write (decide + reserve) or only read the sticky
/// placement. Query paths (prewarm gates, post-Ready re-resolves) must never
/// mint reservations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaceIntent {
    Query,
    Commit,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(u128::from(d.as_secs())))
        .unwrap_or(0)
}

/// Is this env engine-placeable? Only local-placement (or implicit) envs —
/// inline ssh/k8s/provider envs carry their own targets.
pub(crate) fn engine_applicable(cfg: &Config, envc: Option<&EnvConfig>) -> bool {
    cfg.placement.enabled && envc.is_none_or(|e| matches!(e.placement, PlacementMode::Local))
}

/// The zone name a worktree belongs to ("" = unzoned).
fn zone_of(db: &Db, worktree: &str) -> String {
    db.zone_of_worktree(worktree)
        .ok()
        .flatten()
        .map(|z| z.name)
        .unwrap_or_default()
}

/// The resolved placement policy for this worktree's env (zone floor folded).
fn resolved_for(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    envc: Option<&EnvConfig>,
) -> ResolvedPlacement {
    let zone = zone_of(db, worktree);
    let zone_floor = (!zone.is_empty())
        .then(|| cfg.zone.get(&zone).and_then(|z| z.placement_floor))
        .flatten();
    resolve_placement(cfg, envc, zone_floor)
}

/// Record an explicit-pin tenancy (engine on, Commit only): the pin bypasses
/// the broker but its resource floor still counts against the host.
pub(crate) fn note_pin(
    cfg: &Config,
    worktree: &str,
    envc: Option<&EnvConfig>,
    binding: &HostBinding,
    intent: PlaceIntent,
) {
    if intent != PlaceIntent::Commit || !cfg.placement.enabled {
        return;
    }
    let Ok(db) = Db::open() else { return };
    // Idempotent: an existing live row for this worktree is left alone.
    if let Ok(Some(t)) = db.tenancy_for(worktree)
        && t.state != TenancyState::Released
    {
        return;
    }
    let resolved = resolved_for(cfg, &db, worktree, envc);
    let row = TenancyRow {
        sandbox: worktree.to_string(),
        host: binding.id.clone(),
        worktree: worktree.to_string(),
        zone: zone_of(&db, worktree),
        mode: TenancyMode::Pinned,
        req: resolved.req,
        state: TenancyState::Reserved,
        reserved_at: unix_now(),
        activated_at: None,
        released_at: None,
    };
    // best-effort: accounting, never a gate on the pin path
    let _ = db.tenancy_force(&row, unix_now());
}

/// Build the broker's view of one candidate host. Every resource layer the
/// user asked for lives here: DECLARED spec (capacity row / template, else
/// the `[host.<n>] capacity` decl — probe-capped with the safety haircut for
/// independent hosts), RESERVED floors (tenancy ledger), the latest MEASURED
/// sample (refreshed lazily under `headroom_ttl_secs`), and the EFFECTIVE
/// trust class (one notch down for unattested independent hosts).
/// Zero-means-inherit precedence for an overcommit percent: an explicit
/// per-host `overcommit_*_pct` of 0 means "no host override" and falls back to
/// the resolved-placement default; any non-zero value wins.
fn inherit_pct(over: u32, fallback: u32) -> u32 {
    if over == 0 { fallback } else { over }
}

/// Map a host's persisted lifecycle state to the broker's `(ready, failed,
/// draining)` eligibility triple. A retryable failure is NOT terminal (the
/// driver may recover it), so only a non-retryable `Failed` sets `failed`.
fn state_triple(state: Option<&HostState>) -> (bool, bool, bool) {
    match state {
        Some(HostState::Ready) => (true, false, false),
        Some(HostState::Draining) => (false, false, true),
        Some(HostState::Failed(f)) => (false, !f.retryable, false),
        _ => (false, false, false),
    }
}

/// Fold every capacity claim into the broker's `(spec, eff_oc_cpu, eff_oc_mem)`
/// for one host. This is the load-bearing arithmetic the broker never sees (it
/// receives already-folded [`HostSnapshot`]s), extracted pure so it can be
/// unit-tested independent of the DB:
///
/// - **Managed** hosts trust their create-time declared spec at face value;
///   overcommit stays as-is (the broker applies it).
/// - **Independent** hosts compound the claims conservatively —
///   `min(declared, probed) × overcommit × safety` — and then the live
///   MemAvailable gate (`reserved + available×safety`) dominates the static
///   arithmetic. Overcommit is FOLDED into the synthetic spec, so the returned
///   overcommit is 100% (the broker must not double-apply it).
/// - A **failed probe** on an independent host fails CLOSED: `spec = None` ⇒
///   the broker reports `UnknownSpec` ⇒ no packing this round. Dedicated
///   placements (which don't need a spec) are unaffected.
#[allow(clippy::too_many_arguments)]
fn fold_capacity(
    ownership: HostOwnership,
    probe_failed: bool,
    declared: Option<HostSpec>,
    headroom: Option<&thegn_core::host_probe::Headroom>,
    reserved: &thegn_core::capacity::ReservedTotals,
    oc_cpu: u32,
    oc_mem: u32,
    safety_pct: u32,
) -> (Option<HostSpec>, u32, u32) {
    match ownership {
        HostOwnership::Managed => (declared, oc_cpu, oc_mem),
        HostOwnership::Independent if probe_failed => (None, 100, 100),
        HostOwnership::Independent => {
            let ceiling = thegn_core::host_probe::independent_effective_ceiling(
                declared, headroom, oc_cpu, oc_mem, safety_pct,
            )
            .map(|mut c| {
                if let Some(h) = headroom {
                    // Live gate: ceiling' = reserved + available×safety.
                    let allowance =
                        (h.mem_available_kb / 1024) * u64::from(safety_pct.clamp(1, 100)) / 100;
                    c.mem_mb = c.mem_mb.min(reserved.mem_mb + allowance);
                }
                c
            });
            (ceiling, 100, 100)
        }
    }
}

fn snapshot_host(
    db: &Db,
    hc: &HostConfig,
    binding: &HostBinding,
    resolved: &ResolvedPlacement,
    safety_pct: u32,
    headroom_ttl_secs: u64,
    refresh: bool,
) -> HostSnapshot {
    let id = binding.id.clone();
    let mut row = db.host_get(&id).ok().flatten();
    let cap = db.capacity_get(&id).ok().flatten();
    let reserved = db.reserved_totals(&id).unwrap_or_default();
    let tenants = db.tenants_of(&id).unwrap_or_default();
    let ownership = cap
        .as_ref()
        .map(|c| c.ownership)
        .unwrap_or(HostOwnership::Independent);

    // Lazy measured-layer refresh: one cheap exec when the stored sample is
    // stale (never from the idle loop — this runs in blocking placement/CLI
    // contexts only). A failed probe fails CLOSED for packing below.
    let mut probe_failed = false;
    if refresh {
        let stale = row.as_ref().and_then(|r| r.last_headroom).is_none_or(|t| {
            !thegn_core::time_policy::is_fresh(
                unix_now(),
                Some(t),
                headroom_ttl_secs.saturating_add(1),
            )
        });
        if stale {
            match thegn_svc::host::runner_for(&binding.reach)
                .and_then(|mut r| r.probe_headroom().map_err(|e| e.msg))
            {
                Ok(h) => {
                    let now = unix_now();
                    let _ = db.host_set_headroom(&id, &h, now); // best-effort: cache write: headroom bookkeeping; display freshness only
                    // Mirror into the capacity index so every surface
                    // (placement list, panel, ranking) reads one table.
                    let _ = db.capacity_set_measured(
                        // best-effort: cache write: capacity bookkeeping; display freshness only
                        &id,
                        &MeasuredLoad {
                            cpu_milli: u64::from(h.load1_milli),
                            mem_mb: (h.mem_total_kb.saturating_sub(h.mem_available_kb)) / 1024,
                            at: now,
                        },
                        now,
                    );
                    row = db.host_get(&id).ok().flatten();
                }
                Err(_) => probe_failed = true,
            }
        }
    }

    let (ready, failed, draining) = state_triple(row.as_ref().map(|r| &r.state));
    let caps = row.as_ref().and_then(|r| r.caps.clone());
    let declared = cap.as_ref().and_then(|c| c.spec).or(binding.declared_spec);
    let headroom = row.as_ref().and_then(|r| r.headroom);
    let oc_cpu = inherit_pct(
        cap.as_ref().map(|c| c.overcommit_cpu_pct).unwrap_or(0),
        resolved.overcommit_cpu_pct,
    );
    let oc_mem = inherit_pct(
        cap.as_ref().map(|c| c.overcommit_mem_pct).unwrap_or(0),
        resolved.overcommit_mem_pct,
    );
    let (spec, eff_oc_cpu, eff_oc_mem) = fold_capacity(
        ownership,
        probe_failed,
        declared,
        headroom.as_ref(),
        &reserved,
        oc_cpu,
        oc_mem,
        safety_pct,
    );
    let trust = match caps.as_ref() {
        Some(c) => effective_class(c, ownership, hc.trust_egress_enforced),
        None => TrustClass::T0HostShell, // unprobed: no known boundary yet
    };
    HostSnapshot {
        host: id,
        ownership,
        capacity: HostCapacity {
            ownership,
            spec,
            overcommit_cpu_pct: eff_oc_cpu,
            overcommit_mem_pct: eff_oc_mem,
            reserved,
            measured: cap.as_ref().and_then(|c| c.measured),
        },
        ready,
        failed,
        draining,
        trust,
        arch: row.as_ref().and_then(|r| r.arch),
        tenant_zones: tenants
            .iter()
            .map(|t| t.zone.clone())
            .collect::<BTreeSet<_>>(),
        tenants: tenants.len() as u32,
        has_dedicated_tenant: tenants.iter().any(|t| t.mode == TenancyMode::Dedicated),
    }
}

/// Snapshot every engine-visible host: all `[host.*]` entries (config +
/// DB-added, cloud reaches excluded — provider templates are spillover, not
/// machines) with their capacity/tenancy/probe state. `refresh` runs the
/// lazy TTL'd headroom probe (Commit + plan paths; never bare queries).
fn build_snapshot(
    cfg: &Config,
    db: &Db,
    resolved: &ResolvedPlacement,
    refresh: bool,
) -> Vec<HostSnapshot> {
    let mut out = Vec::new();
    for (name, hc) in &cfg.host {
        if hc.reach == HostReach::Cloud {
            continue;
        }
        let Some(binding) = cfg.host_binding(name) else {
            continue; // misconfigured: warned by host_binding
        };
        out.push(snapshot_host(
            db,
            hc,
            &binding,
            resolved,
            cfg.placement.independent_safety_pct,
            cfg.placement.headroom_ttl_secs,
            refresh,
        ));
    }
    out
}

/// The autoscale slice: engine-created host counts per lane + cooling lanes.
fn autoscale_snapshot(db: &Db, now_ms: i64) -> AutoscaleSnapshot {
    let mut s = AutoscaleSnapshot::default();
    if let Ok(rows) = db.capacity_all() {
        for row in rows {
            if row.ownership == HostOwnership::Managed && !row.template.is_empty() {
                s.managed_count += 1;
                *s.lane_counts
                    .entry(format!("tpl:{}/{}", row.provider, row.template))
                    .or_insert(0) += 1;
            }
        }
    }
    if let Ok(markers) = db.health_cooling(now_ms) {
        for m in markers {
            if m.key.starts_with("tpl:") {
                s.cooling.insert(m.key);
            }
        }
    }
    s
}

/// Serialize the per-candidate outcomes for `placement explain`.
fn trace_json(out: &DecisionOutput) -> String {
    #[derive(serde::Serialize)]
    struct Candidate<'a> {
        host: &'a str,
        outcome: &'a str,
    }
    let cands: Vec<Candidate<'_>> = out
        .candidates
        .iter()
        .map(|(h, why)| Candidate {
            host: h.as_str(),
            outcome: why.map(|w| w.as_str()).unwrap_or("eligible"),
        })
        .collect();
    serde_json::to_string(&cands).unwrap_or_default()
}

fn record_event(db: &Db, worktree: &str, decision_tag: &str, chosen: &str, trace: &str) {
    // best-effort: the trace is forensic, never a gate
    let _ = db.placement_event_put(&PlacementEventRow {
        ts: unix_now(),
        worktree: worktree.to_string(),
        decision: decision_tag.to_string(),
        chosen: chosen.to_string(),
        trace_json: trace.to_string(),
    });
}

/// Reserve `worktree` on `host` (mode per the decision). `true` on success.
fn try_reserve(
    db: &Db,
    hosts: &[HostSnapshot],
    host: &HostId,
    worktree: &str,
    zone: &str,
    mode: TenancyMode,
    resolved: &ResolvedPlacement,
) -> bool {
    let Some(snap) = hosts.iter().find(|h| &h.host == host) else {
        return false;
    };
    // Dedicated placements on a spec-less host reserve against a nominal
    // ceiling (exclusivity is the real guarantee; the guarded insert still
    // enforces zone/dedicated conflicts).
    let ceilings = snap.capacity.ceilings().unwrap_or((u64::MAX, u64::MAX));
    let row = TenancyRow {
        sandbox: worktree.to_string(),
        host: host.clone(),
        worktree: worktree.to_string(),
        zone: zone.to_string(),
        mode,
        req: resolved.req,
        state: TenancyState::Reserved,
        reserved_at: unix_now(),
        activated_at: None,
        released_at: None,
    };
    matches!(
        db.tenancy_reserve(&row, ceilings, unix_now()),
        Ok(ReserveOutcome::Reserved) | Ok(ReserveOutcome::AlreadyPlaced(_))
    )
}

fn with_fresh_host(mut hosts: Vec<HostSnapshot>, fresh: HostSnapshot) -> Vec<HostSnapshot> {
    if !hosts.iter().any(|snapshot| snapshot.host == fresh.host) {
        hosts.push(fresh);
    }
    hosts
}

fn fresh_host_unreserved(
    on_exhaustion: OnExhaustion,
    host_name: &str,
    why: &str,
) -> Result<Option<HostBinding>, HostFailure> {
    let msg =
        format!("placement: autoscale created {host_name} but {why}; not creating another host");
    match on_exhaustion {
        OnExhaustion::Error => Err(HostFailure {
            step: HostStep::Connect,
            error: msg,
            retryable: true,
        }),
        OnExhaustion::Queue | OnExhaustion::Reject => {
            thegn_core::msg::warn(&msg);
            Ok(None)
        }
    }
}

fn fresh_provision_failure(
    on_exhaustion: OnExhaustion,
    lane: &str,
    error: &str,
) -> Result<Option<HostBinding>, HostFailure> {
    let reason = format!("provisioning failed ({error})");
    fresh_host_unreserved(on_exhaustion, lane, &reason)
}

/// Binding for a chosen host id (named hosts only — the engine never places
/// onto anonymous or cloud hosts).
fn binding_for(cfg: &Config, host: &HostId) -> Option<HostBinding> {
    cfg.host_binding(host.config_name()?)
}

/// Resolve a live tenancy against the loaded config first, then the DB-backed
/// host catalog. A catalog read failure must never release a live reservation.
#[expect(
    clippy::large_enum_variant,
    reason = "short-lived local result of one lookup; boxing buys nothing"
)]
enum StickyBinding {
    Found(HostBinding),
    Missing,
    Unavailable,
}

fn sticky_binding(
    cfg: &Config,
    db: &Db,
    tenancy: &TenancyRow,
    on_exhaustion: OnExhaustion,
) -> Result<StickyBinding, HostFailure> {
    if let Some(binding) = binding_for(cfg, &tenancy.host) {
        return Ok(StickyBinding::Found(binding));
    }
    let Some(name) = tenancy.host.config_name() else {
        if let Err(error) = db.tenancy_release(&tenancy.sandbox, unix_now()) {
            tracing::debug!(%error, "release tenancy for host missing from config");
        }
        return Ok(StickyBinding::Missing);
    };
    match crate::autoscale::db_host_def(db, name) {
        Ok(Some((_, binding))) => Ok(StickyBinding::Found(binding)),
        Ok(None) => {
            if let Err(error) = db.tenancy_release(&tenancy.sandbox, unix_now()) {
                tracing::debug!(%error, "release tenancy for host missing from config");
            }
            Ok(StickyBinding::Missing)
        }
        Err(error) => {
            let message =
                format!("placement: could not read host {name} for live tenancy: {error:#}");
            match on_exhaustion {
                OnExhaustion::Error => Err(HostFailure {
                    step: HostStep::Connect,
                    error: message,
                    retryable: true,
                }),
                OnExhaustion::Queue | OnExhaustion::Reject => {
                    thegn_core::msg::warn(&message);
                    Ok(StickyBinding::Unavailable)
                }
            }
        }
    }
}

/// Engine placement for one worktree. `Ok(None)` ⇒ not engine-placed (caller
/// falls through to the status-quo path); `Err` ⇒ a loud halt
/// (`on_exhaustion = "error"`).
pub(crate) fn place(
    cfg: &Config,
    worktree: &str,
    envc: Option<&EnvConfig>,
    intent: PlaceIntent,
) -> Result<Option<HostBinding>, HostFailure> {
    if !engine_applicable(cfg, envc) {
        return Ok(None);
    }
    let Ok(db) = Db::open() else {
        return Ok(None); // no state DB ⇒ engine can't account: status quo
    };
    let resolved = resolved_for(cfg, &db, worktree, envc);
    // Sticky: a live tenancy pins this worktree to its host across calls and
    // restarts (the decision is made once, not per resolve).
    if let Ok(Some(t)) = db.tenancy_for(worktree)
        && t.state != TenancyState::Released
    {
        match sticky_binding(cfg, &db, &t, resolved.on_exhaustion)? {
            StickyBinding::Found(binding) => return Ok(Some(binding)),
            StickyBinding::Missing => {}
            StickyBinding::Unavailable => return Ok(None),
        }
    }
    if intent == PlaceIntent::Query {
        return Ok(None);
    }

    if !resolved.enabled {
        return Ok(None);
    }
    let zone = zone_of(&db, worktree);
    let request = PlacementRequest {
        sandbox: worktree.to_string(),
        worktree: worktree.to_string(),
        req: resolved.req,
        mode: resolved.mode,
        zone: zone.clone(),
        required_trust: required_class(profile_of(cfg, envc)),
        arch: None,
    };

    for _round in 0..RESERVE_ROUNDS {
        let hosts = build_snapshot(cfg, &db, &resolved, true);
        let inputs = PlacementInputs {
            hosts: hosts.clone(),
            pack_strategy: resolved.pack_strategy,
            on_exhaustion: resolved.on_exhaustion,
            autoscale: cfg.placement.autoscale.clone(),
            autoscale_state: autoscale_snapshot(&db, unix_now() * 1000),
        };
        let out = decide_placement(
            &request,
            &inputs,
            resolved.overcommit_cpu_pct,
            resolved.overcommit_mem_pct,
        );
        let trace = trace_json(&out);
        match out.decision {
            PlacementDecision::Packed { .. } | PlacementDecision::Dedicated { .. } => {
                let (host, alternates, mode) = match out.decision {
                    PlacementDecision::Packed { host, alternates } => {
                        (host, alternates, TenancyMode::Packed)
                    }
                    PlacementDecision::Dedicated { host, alternates } => {
                        (host, alternates, TenancyMode::Dedicated)
                    }
                    _ => unreachable!("outer match arm"),
                };
                for candidate in std::iter::once(host.clone()).chain(alternates) {
                    if try_reserve(&db, &hosts, &candidate, worktree, &zone, mode, &resolved) {
                        record_event(&db, worktree, mode_tag(mode), candidate.as_str(), &trace);
                        if let Some(b) = binding_for(cfg, &candidate) {
                            return Ok(Some(b));
                        }
                        // Unresolvable binding (config changed mid-flight):
                        // release + fall to the next candidate.
                        let _ = db.tenancy_release(worktree, unix_now()); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                    }
                }
                // Lost every race this round: re-snapshot + re-decide.
                continue;
            }
            PlacementDecision::Provision { template } => {
                // Autoscale is a PAID lane: the compute ledger gates it.
                match budget_verdict(&db, &zone, resolved.on_exhaustion) {
                    ComputeVerdict::Allow => {}
                    verdict => {
                        if try_spillover(cfg, &db, worktree, &zone, resolved.on_exhaustion, &trace)
                        {
                            return Ok(None);
                        }
                        record_event(&db, worktree, "queued", "budget", &trace);
                        return match verdict {
                            ComputeVerdict::Refuse(why)
                                if resolved.on_exhaustion == OnExhaustion::Error =>
                            {
                                Err(HostFailure {
                                    step: HostStep::Connect,
                                    error: format!("placement: {why}"),
                                    retryable: true,
                                })
                            }
                            ComputeVerdict::Refuse(why) => {
                                thegn_core::msg::warn(&format!("placement: {why}"));
                                Ok(None)
                            }
                            _ => {
                                thegn_core::msg::warn(
                                    "placement: compute cap reached; queued (paid lanes paused)",
                                );
                                crate::autoscale::queue_worktree(worktree);
                                Ok(None)
                            }
                        };
                    }
                }
                record_event(&db, worktree, "provision", &template.lane_key(), &trace);
                match crate::autoscale::provision_managed(cfg, &db, &template) {
                    Ok(host_name) => {
                        let (hc, binding) = match crate::autoscale::db_host_def(&db, &host_name) {
                            Ok(Some(found)) => found,
                            Ok(None) => {
                                return fresh_host_unreserved(
                                    resolved.on_exhaustion,
                                    &host_name,
                                    "its host definition is missing",
                                );
                            }
                            Err(error) => {
                                return fresh_host_unreserved(
                                    resolved.on_exhaustion,
                                    &host_name,
                                    &format!("its host definition could not be read ({error:#})"),
                                );
                            }
                        };
                        // The loaded Config predates the create: add the fresh host to the
                        // snapshot so the reservation can land on it.
                        let fresh = snapshot_host(
                            &db,
                            &hc,
                            &binding,
                            &resolved,
                            cfg.placement.independent_safety_pct,
                            cfg.placement.headroom_ttl_secs,
                            false,
                        );
                        let hosts =
                            with_fresh_host(build_snapshot(cfg, &db, &resolved, false), fresh);
                        if try_reserve(
                            &db,
                            &hosts,
                            &binding.id,
                            worktree,
                            &zone,
                            TenancyMode::Packed,
                            &resolved,
                        ) {
                            record_event(
                                &db,
                                worktree,
                                mode_tag(TenancyMode::Packed),
                                binding.id.as_str(),
                                &trace,
                            );
                            return Ok(Some(cfg.host_binding(&host_name).unwrap_or(binding)));
                        }
                        return fresh_host_unreserved(
                            resolved.on_exhaustion,
                            &host_name,
                            "no slot could be reserved on it",
                        );
                    }
                    Err(e) => {
                        let error = format!("{e:#}");
                        let lane = template.lane_key();
                        thegn_core::msg::warn(&format!(
                            "placement: autoscale lane {lane} failed: {e}"
                        ));
                        return fresh_provision_failure(resolved.on_exhaustion, &lane, &error);
                    }
                }
            }
            PlacementDecision::Queue => {
                if try_spillover(cfg, &db, worktree, &zone, resolved.on_exhaustion, &trace) {
                    return Ok(None);
                }
                record_event(&db, worktree, "queued", "", &trace);
                thegn_core::msg::warn(
                    "placement: no eligible host; queued — re-open to place once capacity frees",
                );
                crate::autoscale::queue_worktree(worktree);
                return Ok(None);
            }
            PlacementDecision::Reject => {
                if try_spillover(cfg, &db, worktree, &zone, resolved.on_exhaustion, &trace) {
                    return Ok(None);
                }
                record_event(&db, worktree, "rejected", "", &trace);
                return Ok(None);
            }
            PlacementDecision::Halt => {
                if try_spillover(cfg, &db, worktree, &zone, resolved.on_exhaustion, &trace) {
                    return Ok(None);
                }
                record_event(&db, worktree, "error", "", &trace);
                return Err(HostFailure {
                    step: HostStep::Connect,
                    error: "placement exhausted (on_exhaustion = \"error\"): no eligible \
                            host, autoscale unavailable"
                        .into(),
                    retryable: true,
                });
            }
        }
    }
    // Every round lost its races: honor the exhaustion policy.
    match resolved.on_exhaustion {
        OnExhaustion::Error => Err(HostFailure {
            step: HostStep::Connect,
            error: "placement: lost every reservation race".into(),
            retryable: true,
        }),
        _ => {
            thegn_core::msg::warn("placement: lost every reservation race; falling back");
            Ok(None)
        }
    }
}

fn mode_tag(mode: TenancyMode) -> &'static str {
    match mode {
        TenancyMode::Packed => "packed",
        TenancyMode::Dedicated => "dedicated",
        TenancyMode::Pinned => "pinned",
    }
}

/// The env's resolved hardening profile (base `[sandbox]` + env overlay) —
/// the input to the pack gate's required trust class.
fn profile_of(cfg: &Config, envc: Option<&EnvConfig>) -> thegn_core::config::SandboxProfile {
    envc.and_then(|e| e.sandbox.profile)
        .unwrap_or(cfg.sandbox.profile)
}

/// One candidate row of a dry-run plan (JSON-shaped for the CLI/smoke).
#[derive(Debug, serde::Serialize)]
pub(crate) struct PlanCandidate {
    pub host: String,
    pub outcome: String,
}

/// A dry-run of the broker for one worktree — same inputs as a real
/// placement, zero side effects (no reservation, no event).
#[derive(Debug, serde::Serialize)]
pub(crate) struct PlanOutput {
    pub decision: String,
    pub chosen: String,
    pub candidates: Vec<PlanCandidate>,
}

/// The env selection a placement dry run plans for. A failed read is an error
/// unless no non-local env, pinned host, or placement engine is configured
/// (then the only possible environment is local and the default resolution is truthful).
fn plan_selection(
    read: anyhow::Result<Option<String>>,
    cfg: &Config,
) -> anyhow::Result<Option<String>> {
    match read {
        Ok(selected) => Ok(selected),
        Err(_) if !cfg.placement.enabled
            && !cfg
                .env
                .values()
                .any(|env| env.placement != PlacementMode::Local || !env.host.trim().is_empty()) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.context(
            "the worktree's environment selection could not be read; refusing to plan against the default environment",
        )),
    }
}

/// Pure dry-run for `thegn placement plan`. `None` ⇒ engine off / env not
/// engine-placeable (pinned envs report a `pinned` plan); `Err` ⇒ selection
/// or DB read failed.
pub(crate) fn plan(cfg: &Config, worktree: &str) -> anyhow::Result<Option<PlanOutput>> {
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => return plan_selection(Err(error), cfg).map(|_| None),
    };
    let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    let repo_root = thegn_core::repo::main_worktree(std::path::Path::new(worktree))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let selected = plan_selection(
        db.effective_env_checked(worktree, &repo_root.to_string_lossy()),
        cfg,
    )?;
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        std::path::Path::new(worktree),
        selected.as_deref(),
    );
    let envc = cfg.env.get(&environment.name);
    if let Some(e) = envc.filter(|e| !e.host.trim().is_empty()) {
        let chosen = cfg
            .resolve_host_binding(&environment.name, e)
            .map(|b| b.id.to_string())
            .unwrap_or_default();
        return Ok(Some(PlanOutput {
            decision: "pinned".into(),
            chosen,
            candidates: Vec::new(),
        }));
    }
    if !engine_applicable(cfg, envc) {
        return Ok(None);
    }
    let resolved = resolved_for(cfg, &db, worktree, envc);
    if !resolved.enabled {
        return Ok(None);
    }
    let zone = zone_of(&db, worktree);
    let request = PlacementRequest {
        sandbox: worktree.to_string(),
        worktree: worktree.to_string(),
        req: resolved.req,
        mode: resolved.mode,
        zone,
        required_trust: required_class(profile_of(cfg, envc)),
        arch: None,
    };
    let hosts = build_snapshot(cfg, &db, &resolved, true);
    let inputs = PlacementInputs {
        hosts,
        pack_strategy: resolved.pack_strategy,
        on_exhaustion: resolved.on_exhaustion,
        autoscale: cfg.placement.autoscale.clone(),
        autoscale_state: autoscale_snapshot(&db, unix_now() * 1000),
    };
    let out = decide_placement(
        &request,
        &inputs,
        resolved.overcommit_cpu_pct,
        resolved.overcommit_mem_pct,
    );
    let chosen = match &out.decision {
        PlacementDecision::Packed { host, .. } | PlacementDecision::Dedicated { host, .. } => {
            host.to_string()
        }
        PlacementDecision::Provision { template } => template.lane_key(),
        _ => String::new(),
    };
    Ok(Some(PlanOutput {
        decision: out.decision.tag().to_string(),
        chosen,
        candidates: out
            .candidates
            .iter()
            .map(|(h, why)| PlanCandidate {
                host: h.to_string(),
                outcome: why.map(|w| w.as_str()).unwrap_or("eligible").to_string(),
            })
            .collect(),
    }))
}

/// The paid-lane budget verdict for this worktree's zone.
fn budget_verdict(db: &Db, zone: &str, on_exhaustion: OnExhaustion) -> ComputeVerdict {
    check_compute_budget(
        db,
        (!zone.is_empty()).then_some(zone),
        on_exhaustion == OnExhaustion::Queue,
    )
}

/// The spillover lane: pick the first healthy, budget-clear entry of
/// `[placement] spillover_envs` (provider-placement envs riding the existing
/// pipeline), persist it as the worktree's env selection, and let the
/// materialize continue — downstream provisioning re-resolves the env and
/// lands on the vendor. Sticky: the selection persists for the worktree's
/// life; fail-back affects new placements only.
fn try_spillover(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    zone: &str,
    on_exhaustion: OnExhaustion,
    trace: &str,
) -> bool {
    let order = &cfg.placement.spillover_envs;
    if order.is_empty() {
        return false;
    }
    let now_ms = thegn_core::util::now_ms();
    let marker_for = |env: &str| -> Option<thegn_core::spillover::SpillState> {
        let m = db.health_get(&format!("provider:{env}")).ok().flatten()?;
        Some(thegn_core::spillover::SpillState {
            kind: thegn_core::spillover::SpillKind::parse(&m.kind)
                .unwrap_or(thegn_core::spillover::SpillKind::CreateFailure),
            retry_at_ms: m.retry_at_ms,
        })
    };
    let budget_ok = |_env: &str| budget_verdict(db, zone, on_exhaustion) == ComputeVerdict::Allow;
    let picked = thegn_core::spillover::pick_spillover(order, &marker_for, &budget_ok, now_ms)
        .map(str::to_string);
    let Some(env) = picked else { return false };
    // A spillover env must actually exist with a provider placement.
    if !cfg
        .env
        .get(&env)
        .is_some_and(|e| matches!(e.placement, PlacementMode::Provider))
    {
        thegn_core::msg::warn(&format!(
            "placement: spillover env {env:?} is not a [env.*] provider placement; skipping"
        ));
        return false;
    }
    if db.set_worktree_env(worktree, &env).is_err() {
        return false;
    }
    record_event(db, worktree, "spillover", &format!("env:{env}"), trace);
    thegn_core::msg::info(&format!(
        "placement: pool exhausted — spilling {worktree} to provider env {env}"
    ));
    true
}

/// Record a spillover env's provisioning failure so the picker cools it down
/// (or parks it budget-dead) — called from the provision error path.
pub(crate) fn note_spillover_failure(cfg: &Config, worktree: &str, error: &str) {
    let Ok(db) = Db::open() else { return };
    let env = db.effective_env(worktree, "").unwrap_or_default();
    if !cfg.placement.spillover_envs.contains(&env) {
        return;
    }
    let kind = thegn_core::spillover::classify_spill(None, error);
    let key = format!("provider:{env}");
    let consecutive = db
        .health_get(&key)
        .ok()
        .flatten()
        .map(|m| m.consecutive.saturating_add(1))
        .unwrap_or(1);
    let now_ms = thegn_core::util::now_ms();
    let cooldown = thegn_core::spillover::spill_cooldown_ms(kind, consecutive, None);
    // best-effort: health is advisory, provisioning already surfaced the error
    let _ = db.health_mark(&HealthMarker {
        key,
        kind: kind.as_str().to_string(),
        reason: error.chars().take(200).collect(),
        since_ms: now_ms,
        retry_at_ms: now_ms.saturating_add(cooldown),
        consecutive,
    });
}

/// Mark this worktree's tenancy `active` (provision reached its marker).
pub(crate) fn mark_active(worktree: &str) {
    if let Ok(db) = Db::open() {
        // best-effort: the sweep tolerates a missed activation
        let _ = db.tenancy_activate(worktree, unix_now());
    }
}

/// Release this worktree's tenancy (provision failure / worktree delete).
pub(crate) fn release(worktree: &str) {
    if let Ok(db) = Db::open() {
        // best-effort: the sweep is the backstop
        let _ = db.tenancy_release(worktree, unix_now());
    }
}

/// Maintainer-tick sweep: release reservations whose drivers died.
pub(crate) fn sweep_stale() {
    if let Ok(db) = Db::open()
        && let Ok(n) = db.tenancy_sweep_stale(unix_now() - RESERVATION_TTL_SECS)
        && n > 0
    {
        tracing::info!(target: "thegn::placement", swept = n, "released stale reservations");
    }
}

/// Housekeeping entry from the hydration thread (the `vps_reaper::tick`
/// pattern): self-throttled, free when the engine is off, all real work on
/// its own thread — stale-reservation sweep, drained-host scale-down, and
/// queued-spawn nudges.
pub(crate) fn maintain_tick(cfg: &Config) {
    if !cfg.placement.enabled {
        return;
    }
    const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < TICK_INTERVAL) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let cfg = cfg.clone();
    thegn_core::thread_name::spawn_named("thegn-placement", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background); // Background: periodic placement maintenance.
        sweep_stale();
        if let Ok(db) = Db::open() {
            use thegn_core::store::ComputeLedgerStore;
            thegn_core::zone::sync_compute_budget_caps(&cfg, &db);
            // Watermark accrual: idempotent, catch-up-correct — cadence only
            // affects display freshness, never totals.
            let now_ms = thegn_core::util::now_ms();
            for m in db.live_compute_meters().unwrap_or_default() {
                let _ = db.accrue_compute_meter(&m.resource, now_ms); // best-effort: cache write: meter accrual is ledger bookkeeping; display freshness only
            }
        }
        crate::autoscale::scaledown_tick(&cfg);
        for wt in crate::autoscale::nudge_queued() {
            thegn_core::msg::info(&format!(
                "placement: capacity may have freed — re-open {wt} to place it"
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::capacity::ReservedTotals;
    use thegn_core::host::HostStep;
    use thegn_core::host_machine::HostState;
    use thegn_core::host_probe::Headroom;
    use thegn_core::store::HostCapacityRow;

    fn spec(cpu_milli: u32, mem_mb: u64) -> HostSpec {
        HostSpec { cpu_milli, mem_mb }
    }

    /// A headroom sample: `mem_total_kb`/`mem_available_kb` in KiB.
    fn headroom(cpus: u32, mem_total_kb: u64, mem_available_kb: u64) -> Headroom {
        Headroom {
            cpus,
            mem_total_kb,
            mem_available_kb,
            load1_milli: 0,
            disk_free_bytes: 0,
            containers_running: 0,
        }
    }

    fn reserved(cpu_milli: u64, mem_mb: u64) -> ReservedTotals {
        ReservedTotals {
            cpu_milli,
            mem_mb,
            tenants: 0,
        }
    }

    fn managed_snapshot(host: HostId) -> HostSnapshot {
        HostSnapshot {
            host,
            ownership: HostOwnership::Managed,
            capacity: HostCapacity {
                ownership: HostOwnership::Managed,
                spec: None,
                overcommit_cpu_pct: 100,
                overcommit_mem_pct: 100,
                reserved: ReservedTotals::default(),
                measured: None,
            },
            ready: false,
            failed: false,
            draining: false,
            trust: TrustClass::T0HostShell,
            arch: None,
            tenant_zones: BTreeSet::new(),
            tenants: 0,
            has_dedicated_tenant: false,
        }
    }

    #[test]
    fn inherit_pct_zero_falls_back() {
        // An explicit 0 (no host override) inherits the resolved default...
        assert_eq!(inherit_pct(0, 150), 150);
        // ...but any non-zero host value wins over the default.
        assert_eq!(inherit_pct(200, 150), 200);
        assert_eq!(inherit_pct(100, 0), 100);
        // Zero on both is still zero (the callee normalizes 0→100 downstream).
        assert_eq!(inherit_pct(0, 0), 0);
    }

    #[test]
    fn state_triple_maps_every_case() {
        assert_eq!(state_triple(Some(&HostState::Ready)), (true, false, false));
        assert_eq!(
            state_triple(Some(&HostState::Draining)),
            (false, false, true)
        );
        // Non-retryable failure is terminal ⇒ failed.
        let fatal = HostState::Failed(HostFailure {
            step: HostStep::Connect,
            error: "boom".into(),
            retryable: false,
        });
        assert_eq!(state_triple(Some(&fatal)), (false, true, false));
        // Retryable failure is NOT terminal — the driver may recover it.
        let transient = HostState::Failed(HostFailure {
            step: HostStep::Connect,
            error: "temporary".into(),
            retryable: true,
        });
        assert_eq!(state_triple(Some(&transient)), (false, false, false));
        // Transient / unknown / absent ⇒ neither ready nor failed nor draining.
        assert_eq!(
            state_triple(Some(&HostState::Probing)),
            (false, false, false)
        );
        assert_eq!(state_triple(None), (false, false, false));
    }

    #[test]
    fn managed_trusts_declared_spec_and_overcommit() {
        // Managed: the create-time spec is authoritative; overcommit passes
        // through untouched (the broker applies it) and the probe is ignored.
        let (s, cpu, mem) = fold_capacity(
            HostOwnership::Managed,
            false,
            Some(spec(4000, 8192)),
            None,
            &reserved(0, 0),
            150,
            120,
            85,
        );
        assert_eq!(s, Some(spec(4000, 8192)));
        assert_eq!((cpu, mem), (150, 120));
    }

    #[test]
    fn managed_probe_failure_is_ignored() {
        // A failed probe never fails-closed a Managed host: its spec stands.
        let (s, cpu, mem) = fold_capacity(
            HostOwnership::Managed,
            true,
            Some(spec(2000, 4096)),
            None,
            &reserved(0, 0),
            100,
            100,
            85,
        );
        assert_eq!(s, Some(spec(2000, 4096)));
        assert_eq!((cpu, mem), (100, 100));
    }

    #[test]
    fn independent_probe_failure_fails_closed() {
        // Independent + probe_failed ⇒ spec None (⇒ broker UnknownSpec ⇒ no
        // packing) and overcommit collapses to 100 (folded, not double-applied).
        let (s, cpu, mem) = fold_capacity(
            HostOwnership::Independent,
            true,
            Some(spec(9999, 99999)), // even a fat declared spec is dropped
            Some(&headroom(8, 32_768_000, 16_384_000)),
            &reserved(0, 0),
            100,
            100,
            85,
        );
        assert_eq!(s, None);
        assert_eq!((cpu, mem), (100, 100));
    }

    #[test]
    fn independent_folds_overcommit_into_spec() {
        // No probe, only a declared spec: independent_effective_ceiling applies
        // overcommit×safety, and the returned overcommit is 100 (folded in).
        let (s, cpu, mem) = fold_capacity(
            HostOwnership::Independent,
            false,
            Some(spec(4000, 8192)),
            None,
            &reserved(0, 0),
            100,
            100,
            100, // safety 100 ⇒ no haircut, overcommit 100 ⇒ no burst
        );
        assert_eq!(s, Some(spec(4000, 8192)));
        assert_eq!((cpu, mem), (100, 100));
    }

    #[test]
    fn independent_no_declared_no_probe_is_dedicated_only() {
        // Neither a declared spec nor a probe ⇒ spec None ⇒ dedicated-only.
        let (s, cpu, mem) = fold_capacity(
            HostOwnership::Independent,
            false,
            None,
            None,
            &reserved(0, 0),
            100,
            100,
            85,
        );
        assert_eq!(s, None);
        assert_eq!((cpu, mem), (100, 100));
    }

    #[test]
    fn independent_live_mem_gate_clamps_below_static_ceiling() {
        // Declared 16 GiB, but only 2 GiB is live-available. At safety 100 the
        // static ceiling would be 16384 MiB; the live gate clamps mem to
        // reserved(0) + available(2048)×100% = 2048 MiB.
        let (s, _cpu, _mem) = fold_capacity(
            HostOwnership::Independent,
            false,
            Some(spec(8000, 16384)),
            // probed spec matches declared on cpu/mem-total so only the
            // available-mem gate bites: total 16 GiB, available 2 GiB.
            Some(&headroom(8, 16_384 * 1024, 2048 * 1024)),
            &reserved(0, 0),
            100,
            100,
            100,
        );
        let s = s.expect("spec present with a declared+probed host");
        assert_eq!(s.mem_mb, 2048, "live MemAvailable gate must dominate");
    }

    #[test]
    fn independent_live_gate_adds_reserved_floor() {
        // The live gate floor is reserved + available×safety — reserved tenants
        // still hold their claim, so the ceiling rises with them (the min()
        // must not drop the reserved term).
        let with_reserved = fold_capacity(
            HostOwnership::Independent,
            false,
            Some(spec(8000, 16384)),
            Some(&headroom(8, 16_384 * 1024, 2048 * 1024)),
            &reserved(0, 4096), // 4 GiB already reserved
            100,
            100,
            100,
        )
        .0
        .expect("spec present");
        // ceiling' = min(static 16384, reserved 4096 + available 2048) = 6144.
        assert_eq!(with_reserved.mem_mb, 6144);
    }

    #[test]
    fn independent_safety_haircut_shrinks_live_allowance() {
        // safety_pct halves the live allowance: available 4 GiB × 50% = 2 GiB.
        let (s, _cpu, _mem) = fold_capacity(
            HostOwnership::Independent,
            false,
            Some(spec(8000, 16384)),
            Some(&headroom(8, 16_384 * 1024, 4096 * 1024)),
            &reserved(0, 0),
            100,
            100,
            50,
        );
        assert_eq!(s.expect("spec").mem_mb, 2048);
    }

    #[test]
    fn fresh_host_joins_the_reservation_snapshot() {
        let first = managed_snapshot(HostId::named("host-a"));
        let fresh = managed_snapshot(HostId::named("host-b"));

        let joined = with_fresh_host(vec![first], fresh.clone());

        assert_eq!(joined.len(), 2);
        assert_eq!(joined.last(), Some(&fresh));
    }

    #[test]
    fn fresh_host_is_not_duplicated_in_the_snapshot() {
        let existing = managed_snapshot(HostId::named("host-a"));

        let joined = with_fresh_host(vec![existing.clone()], existing.clone());

        assert_eq!(joined.len(), 1);
        assert_eq!(joined.first(), Some(&existing));
    }

    #[test]
    fn fresh_managed_host_is_reservable_once_in_the_snapshot() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let name = "tg-auto-cx32-0000000000000001";
        let hc = HostConfig {
            reach: HostReach::Local,
            ..HostConfig::default()
        };
        db.put_host_def(name, &hc, 1).unwrap();
        db.capacity_put(&HostCapacityRow {
            host: HostId::named(name),
            ownership: HostOwnership::Managed,
            spec: Some(HostSpec {
                cpu_milli: 64_000,
                mem_mb: 262_144,
            }),
            overcommit_cpu_pct: 0,
            overcommit_mem_pct: 0,
            provider: "hetzner".into(),
            template: "cx32".into(),
            created_at: Some(1),
            measured: None,
            updated_at: 1,
        })
        .unwrap();
        let (hc, binding) = crate::autoscale::db_host_def(&db, name).unwrap().unwrap();
        let resolved = resolve_placement(&Config::default(), None, None);

        assert!(!try_reserve(
            &db,
            &[],
            &binding.id,
            "/wt/a",
            "",
            TenancyMode::Packed,
            &resolved,
        ));
        let hosts = with_fresh_host(
            Vec::new(),
            snapshot_host(&db, &hc, &binding, &resolved, 100, 60, false),
        );
        assert!(try_reserve(
            &db,
            &hosts,
            &binding.id,
            "/wt/a",
            "",
            TenancyMode::Packed,
            &resolved,
        ));
    }

    #[test]
    fn fresh_host_sticky_binding_survives_a_stale_config() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let name = "tg-auto-cx32-0000000000000001";
        let host_config = HostConfig {
            reach: HostReach::Local,
            ..HostConfig::default()
        };
        db.put_host_def(name, &host_config, 1).unwrap();

        let cfg = Config::default();
        let resolved = resolve_placement(&cfg, None, None);
        let tenancy = TenancyRow {
            sandbox: "/wt/fresh".into(),
            host: HostId::named(name),
            worktree: "/wt/fresh".into(),
            zone: String::new(),
            mode: TenancyMode::Packed,
            req: resolved.req,
            state: TenancyState::Reserved,
            reserved_at: 1,
            activated_at: None,
            released_at: None,
        };
        db.tenancy_force(&tenancy, 1).unwrap();

        let binding = sticky_binding(&cfg, &db, &tenancy, resolved.on_exhaustion).unwrap();

        assert!(matches!(binding, StickyBinding::Found(found) if found.id == tenancy.host));
        assert_eq!(
            db.tenancy_for("/wt/fresh").unwrap().unwrap().state,
            TenancyState::Reserved
        );
    }

    #[test]
    fn fresh_host_unreserved_honours_on_exhaustion() {
        let host_name = "tg-auto-cx32-0000000000000001";
        let refused = fresh_host_unreserved(
            OnExhaustion::Error,
            host_name,
            "provisioning failed (resolve fresh host ip)",
        );
        assert!(matches!(refused, Err(failure)
            if failure.error.contains(host_name)
                && failure.error.contains("provisioning failed")
                && failure.error.contains("not creating another host")));

        for policy in [OnExhaustion::Queue, OnExhaustion::Reject] {
            assert!(matches!(
                fresh_host_unreserved(policy, host_name, "provisioning failed"),
                Ok(None)
            ));
        }
    }

    #[test]
    fn fresh_provision_failure_returns_once_with_lane_context() {
        let lane = "tpl:hetzner/cx32";
        let refused = fresh_provision_failure(
            OnExhaustion::Error,
            lane,
            "resolve fresh host ip: provider unavailable",
        );
        assert!(matches!(refused, Err(failure)
            if failure.error.contains(lane)
                && failure.error.contains("provisioning failed")
                && failure.error.contains("provider unavailable")
                && failure.error.contains("not creating another host")));

        assert!(matches!(
            fresh_provision_failure(OnExhaustion::Queue, lane, "provider unavailable"),
            Ok(None)
        ));
    }

    #[test]
    fn plan_honours_the_worktrees_selected_env() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt").to_string_lossy().into_owned();
        db.put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
            .unwrap();
        db.set_worktree_env(&wt, "pinned").unwrap();
        let cfg = Config {
            env: std::collections::BTreeMap::from([(
                "pinned".to_string(),
                EnvConfig {
                    host: "h1".into(),
                    ..EnvConfig::default()
                },
            )]),
            ..Config::default()
        };

        assert_eq!(
            plan(&cfg, &wt).unwrap().map(|plan| plan.decision),
            Some("pinned".to_string())
        );
    }

    #[test]
    fn plan_selection_refuses_an_unreadable_selection_when_a_provider_env_is_configured() {
        let cfg = Config {
            env: std::collections::BTreeMap::from([(
                "box".to_string(),
                EnvConfig {
                    placement: PlacementMode::Provider,
                    ..EnvConfig::default()
                },
            )]),
            ..Config::default()
        };

        let error = plan_selection(Err(anyhow::anyhow!("database is locked")), &cfg)
            .expect_err("provider env makes the selection read authoritative");
        let message = format!("{error:#}");
        assert!(message.contains("could not be read"), "{message}");
        assert!(message.contains("database is locked"), "{message}");
    }

    #[test]
    fn plan_selection_refuses_an_unreadable_selection_when_placement_is_enabled() {
        let cfg = Config {
            placement: thegn_core::config::PlacementConfig {
                enabled: true,
                ..thegn_core::config::PlacementConfig::default()
            },
            ..Config::default()
        };

        assert!(plan_selection(Err(anyhow::anyhow!("database is locked")), &cfg).is_err());
    }

    #[test]
    fn plan_selection_allows_a_local_only_config() {
        let cfg = Config::default();

        assert_eq!(
            plan_selection(Err(anyhow::anyhow!("database is locked")), &cfg).unwrap(),
            None
        );
    }

    #[test]
    fn plan_selection_passes_a_read_through() {
        let cfg = Config::default();

        assert_eq!(
            plan_selection(Ok(Some("x".into())), &cfg).unwrap(),
            Some("x".into())
        );
        assert_eq!(plan_selection(Ok(None), &cfg).unwrap(), None);
    }
}
