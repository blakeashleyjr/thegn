//! The placement **broker** — one pure, total, deterministic decision function
//! over snapshot inputs, in the house style of [`crate::host_machine::step`]
//! and `render_plan::plan`: trust gates run **before** any cost/capacity
//! ranking, every ineligibility is a typed reason (never a boolean), and the
//! same snapshot always produces the same decision. All I/O — snapshotting,
//! the atomic reservation, provisioning — lives in the host crate's drivers.
//!
//! Lane order (fixed):
//! ```text
//! trust gate → dedicated (when the mode demands it) → packed (ranked)
//!   → autoscale managed (ordered template lanes, cooldown-skipped)
//!   → on_exhaustion (queue / reject / halt)
//! ```
//! (Pin bypass happens in the flow layer before the broker is consulted;
//! spillover slots in between autoscale and exhaustion in a later change.)

use std::collections::BTreeSet;

use crate::capacity::{HostCapacity, HostOwnership, ResourceReq, fits, utilization_permille};
use crate::config_placement::{
    AutoscaleConfig, ManagedTemplate, OnExhaustion, PackStrategy, PlacementModePref,
};
use crate::host::{Arch, HostId};

/// One sandbox spawn's placement ask (already resolved + clamped — the broker
/// never re-derives config or trust).
#[derive(Debug, Clone, PartialEq)]
pub struct PlacementRequest {
    /// Resolved sandbox name (the tenancy key).
    pub sandbox: String,
    pub worktree: String,
    pub req: ResourceReq,
    /// The clamped mode ([`crate::config_placement::resolve_placement`]).
    pub mode: PlacementModePref,
    /// Zone co-tenancy key; empty = unzoned (its own class).
    pub zone: String,
    /// The resolved sandbox profile is sealed-class — multi-tenant packing
    /// then requires a rootless runtime on the host.
    pub sealed: bool,
    /// Required architecture (`None` = any).
    pub arch: Option<Arch>,
}

/// What the broker knows about one candidate host, snapshotted by the flow
/// layer from `host_capacity` + `host_tenancy` + `hosts`.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSnapshot {
    pub host: HostId,
    pub ownership: HostOwnership,
    pub capacity: HostCapacity,
    /// `Ready` in the host state machine (preferred in ranking; a non-ready
    /// usable host is still eligible — `ensure_ready` drives it).
    pub ready: bool,
    /// The host is terminally failed (`Failed{retryable:false}`) — out.
    pub failed: bool,
    /// Being drained (de-registration in a later change) — no new placements.
    pub draining: bool,
    /// A probed (or image-built) OCI runtime exists — packing IS remote OCI.
    pub oci_runtime: bool,
    /// The runtime is rootless (sealed-class co-tenancy requirement).
    pub rootless: bool,
    pub arch: Option<Arch>,
    /// Distinct zones of live tenants ("" = an unzoned tenant).
    pub tenant_zones: BTreeSet<String>,
    /// Live tenant count (reserved + active).
    pub tenants: u32,
    pub has_dedicated_tenant: bool,
}

/// Why a host was passed over — the vocabulary `placement explain` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ineligible {
    NotReady,
    Draining,
    TrustClass,
    ZoneCoTenancy,
    DedicatedOccupied,
    Occupied,
    NoCapacity,
    WrongArch,
    UnknownSpec,
}

impl Ineligible {
    pub fn as_str(self) -> &'static str {
        match self {
            Ineligible::NotReady => "not_ready",
            Ineligible::Draining => "draining",
            Ineligible::TrustClass => "trust_class",
            Ineligible::ZoneCoTenancy => "zone_co_tenancy",
            Ineligible::DedicatedOccupied => "dedicated_occupied",
            Ineligible::Occupied => "occupied",
            Ineligible::NoCapacity => "no_capacity",
            Ineligible::WrongArch => "wrong_arch",
            Ineligible::UnknownSpec => "unknown_spec",
        }
    }
}

/// Gates shared by every placement class (state, drain, arch).
fn base_eligible(h: &HostSnapshot, r: &PlacementRequest) -> Result<(), Ineligible> {
    if h.failed {
        return Err(Ineligible::NotReady);
    }
    if h.draining {
        return Err(Ineligible::Draining);
    }
    if let (Some(want), Some(have)) = (r.arch, h.arch)
        && want != have
    {
        return Err(Ineligible::WrongArch);
    }
    Ok(())
}

/// May `r` be PACKED onto `h`? Trust before cost: the isolation gate and zone
/// co-tenancy run ahead of the capacity check, so a full-but-trusted host
/// reads `NoCapacity` while an empty-but-untrusted one reads `TrustClass`.
pub fn pack_eligible(h: &HostSnapshot, r: &PlacementRequest) -> Result<(), Ineligible> {
    base_eligible(h, r)?;
    // Packing is remote-OCI by construction; sealed-class tenants additionally
    // require a rootless runtime as the co-tenancy boundary.
    if !h.oci_runtime || (r.sealed && !h.rootless) {
        return Err(Ineligible::TrustClass);
    }
    if h.has_dedicated_tenant {
        return Err(Ineligible::DedicatedOccupied);
    }
    if h.tenant_zones.iter().any(|z| z != &r.zone) {
        return Err(Ineligible::ZoneCoTenancy);
    }
    if h.capacity.spec.is_none() {
        return Err(Ineligible::UnknownSpec);
    }
    if !fits(&h.capacity, &r.req) {
        return Err(Ineligible::NoCapacity);
    }
    Ok(())
}

/// May `r` take `h` as a DEDICATED host? (Exclusive: the host must be empty.
/// No spec/fits requirement — exclusivity is the resource guarantee.)
pub fn dedicated_eligible(h: &HostSnapshot, r: &PlacementRequest) -> Result<(), Ineligible> {
    base_eligible(h, r)?;
    if h.tenants > 0 || h.has_dedicated_tenant {
        return Err(Ineligible::Occupied);
    }
    Ok(())
}

/// Rank pack-eligible hosts. `BinPack` consolidates (most-utilized first, so
/// idle hosts drain and scale down); `Spread` load-balances (least-utilized
/// first). Ready hosts always sort ahead of not-yet-Ready ones (a decision
/// should not pay a provisioning wait it can avoid); ties break on the host
/// id, so the order is total and deterministic.
pub fn rank_hosts<'a>(
    eligible: impl IntoIterator<Item = &'a HostSnapshot>,
    strategy: PackStrategy,
) -> Vec<HostId> {
    let mut hosts: Vec<&HostSnapshot> = eligible.into_iter().collect();
    hosts.sort_by(|a, b| {
        let ready = b.ready.cmp(&a.ready); // Ready first
        let (ua, ub) = (
            utilization_permille(&a.capacity),
            utilization_permille(&b.capacity),
        );
        let util = match strategy {
            PackStrategy::BinPack => ub.cmp(&ua), // most-utilized first
            PackStrategy::Spread => ua.cmp(&ub),  // least-utilized first
        };
        ready.then(util).then_with(|| a.host.cmp(&b.host))
    });
    hosts.into_iter().map(|h| h.host.clone()).collect()
}

/// The autoscale slice of the snapshot: how many engine-created hosts exist
/// overall and per lane, and which lanes are cooling down.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutoscaleSnapshot {
    /// Engine-created (Managed, `sz-placement=managed`) hosts alive now.
    pub managed_count: u32,
    /// Live hosts per lane key ([`ManagedTemplate::lane_key`]).
    pub lane_counts: std::collections::BTreeMap<String, u32>,
    /// Lane keys currently under a `placement_health` cooldown.
    pub cooling: BTreeSet<String>,
}

/// Everything the broker sees for one decision.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacementInputs {
    pub hosts: Vec<HostSnapshot>,
    pub pack_strategy: PackStrategy,
    pub on_exhaustion: OnExhaustion,
    pub autoscale: AutoscaleConfig,
    pub autoscale_state: AutoscaleSnapshot,
}

/// The broker's verdict. `Packed.alternates` carries the remaining ranked
/// hosts so the flow's reserve-race loop can walk them without re-deciding.
#[derive(Debug, Clone, PartialEq)]
pub enum PlacementDecision {
    Dedicated {
        host: HostId,
        alternates: Vec<HostId>,
    },
    Packed {
        host: HostId,
        alternates: Vec<HostId>,
    },
    /// Create a new Managed host from this template lane, then place there.
    Provision { template: ManagedTemplate },
    /// Park: fall through once, nudge when capacity frees.
    Queue,
    /// Fall back to the env's non-engine path.
    Reject,
    /// Halt loudly (surfaced like a SandboxHalt).
    Halt,
}

impl PlacementDecision {
    /// Terse tag for `placement_events.decision`.
    pub fn tag(&self) -> &'static str {
        match self {
            PlacementDecision::Dedicated { .. } => "dedicated",
            PlacementDecision::Packed { .. } => "packed",
            PlacementDecision::Provision { .. } => "provision",
            PlacementDecision::Queue => "queued",
            PlacementDecision::Reject => "rejected",
            PlacementDecision::Halt => "error",
        }
    }
}

/// The decision plus the per-candidate outcomes (`None` = eligible) — the
/// trace `placement explain` renders.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionOutput {
    pub decision: PlacementDecision,
    pub candidates: Vec<(HostId, Option<Ineligible>)>,
}

/// Would a fresh, empty host with this template's spec fit the request under
/// the given overcommit? (A lane whose plan is too small can never serve.)
fn template_fits(
    t: &ManagedTemplate,
    r: &PlacementRequest,
    oc_cpu_pct: u32,
    oc_mem_pct: u32,
) -> bool {
    let Some(spec) = t.spec() else { return false };
    fits(
        &HostCapacity {
            ownership: HostOwnership::Managed,
            spec: Some(spec),
            overcommit_cpu_pct: oc_cpu_pct,
            overcommit_mem_pct: oc_mem_pct,
            reserved: Default::default(),
            measured: None,
        },
        &r.req,
    )
}

/// THE broker. Pure and total: every input combination yields a decision.
///
/// `oc_cpu_pct`/`oc_mem_pct` are the resolved overcommit percentages, used
/// only for the template-fits check (each snapshot's own capacity already
/// carries its effective overcommit).
pub fn decide_placement(
    r: &PlacementRequest,
    i: &PlacementInputs,
    oc_cpu_pct: u32,
    oc_mem_pct: u32,
) -> DecisionOutput {
    let mut candidates: Vec<(HostId, Option<Ineligible>)> = Vec::new();

    // ── dedicated lane (when the clamped mode demands exclusivity) ─────────
    if r.mode == PlacementModePref::Dedicated {
        let mut eligible = Vec::new();
        for h in &i.hosts {
            match dedicated_eligible(h, r) {
                Ok(()) => {
                    candidates.push((h.host.clone(), None));
                    eligible.push(h);
                }
                Err(why) => candidates.push((h.host.clone(), Some(why))),
            }
        }
        // An exclusive host is "packed onto" nobody: prefer Ready, then the
        // SMALLEST adequate box (spread of a dedicated host is meaningless).
        let mut ranked = rank_hosts(eligible.iter().copied(), PackStrategy::Spread);
        if !ranked.is_empty() {
            let host = ranked.remove(0);
            return DecisionOutput {
                decision: PlacementDecision::Dedicated {
                    host,
                    alternates: ranked,
                },
                candidates,
            };
        }
        return exhaust(r, i, candidates, oc_cpu_pct, oc_mem_pct);
    }

    // ── packed lane ─────────────────────────────────────────────────────────
    let mut eligible = Vec::new();
    for h in &i.hosts {
        match pack_eligible(h, r) {
            Ok(()) => {
                candidates.push((h.host.clone(), None));
                eligible.push(h);
            }
            Err(why) => candidates.push((h.host.clone(), Some(why))),
        }
    }
    let mut ranked = rank_hosts(eligible.iter().copied(), i.pack_strategy);
    if !ranked.is_empty() {
        let host = ranked.remove(0);
        return DecisionOutput {
            decision: PlacementDecision::Packed {
                host,
                alternates: ranked,
            },
            candidates,
        };
    }

    // `auto` may fall back to an exclusive empty host before provisioning
    // anything new (an empty host is already paid for).
    if r.mode == PlacementModePref::Auto {
        let empties: Vec<&HostSnapshot> = i
            .hosts
            .iter()
            .filter(|h| dedicated_eligible(h, r).is_ok())
            .collect();
        let mut ranked = rank_hosts(empties, PackStrategy::Spread);
        if !ranked.is_empty() {
            let host = ranked.remove(0);
            return DecisionOutput {
                decision: PlacementDecision::Dedicated {
                    host,
                    alternates: ranked,
                },
                candidates,
            };
        }
    }

    exhaust(r, i, candidates, oc_cpu_pct, oc_mem_pct)
}

/// The tail of the pipeline: autoscale lanes, then the exhaustion policy.
fn exhaust(
    r: &PlacementRequest,
    i: &PlacementInputs,
    candidates: Vec<(HostId, Option<Ineligible>)>,
    oc_cpu_pct: u32,
    oc_mem_pct: u32,
) -> DecisionOutput {
    let a = &i.autoscale;
    let s = &i.autoscale_state;
    if a.enabled && s.managed_count < a.effective_max_hosts() {
        for t in &a.managed {
            let key = t.lane_key();
            if s.cooling.contains(&key) {
                continue;
            }
            if s.lane_counts.get(&key).copied().unwrap_or(0) >= t.effective_max() {
                continue;
            }
            if !template_fits(t, r, oc_cpu_pct, oc_mem_pct) {
                continue;
            }
            return DecisionOutput {
                decision: PlacementDecision::Provision {
                    template: t.clone(),
                },
                candidates,
            };
        }
    }
    let decision = match i.on_exhaustion {
        OnExhaustion::Queue => PlacementDecision::Queue,
        OnExhaustion::Reject => PlacementDecision::Reject,
        OnExhaustion::Error => PlacementDecision::Halt,
    };
    DecisionOutput {
        decision,
        candidates,
    }
}

/// One host as scale-down sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ScaleDownHost {
    pub host: HostId,
    pub ownership: HostOwnership,
    /// Live tenants (reserved + active — a parked pool spare counts, which is
    /// exactly why its host is kept warm).
    pub tenants: u32,
    /// Engine-created (autoscaled) host — the only kind scale-down may touch.
    pub engine_created: bool,
    pub idle_secs: u64,
}

/// Which engine-created Managed hosts to destroy: zero tenants, idle past the
/// threshold, and never below `min_hosts` engine-created survivors. The
/// longest-idle die first; Independent hosts are structurally never returned.
pub fn decide_scaledown(
    hosts: &[ScaleDownHost],
    idle_threshold_secs: u64,
    min_hosts: u32,
) -> Vec<HostId> {
    let engine_total = hosts.iter().filter(|h| h.engine_created).count() as u32;
    let mut victims: Vec<&ScaleDownHost> = hosts
        .iter()
        .filter(|h| {
            h.engine_created
                && h.ownership == HostOwnership::Managed
                && h.tenants == 0
                && h.idle_secs > idle_threshold_secs
        })
        .collect();
    victims.sort_by(|a, b| {
        b.idle_secs
            .cmp(&a.idle_secs)
            .then_with(|| a.host.cmp(&b.host))
    });
    let killable = engine_total.saturating_sub(min_hosts) as usize;
    victims
        .into_iter()
        .take(killable)
        .map(|h| h.host.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capacity::{HostSpec, ReservedTotals};

    fn snap(name: &str) -> HostSnapshot {
        HostSnapshot {
            host: HostId::named(name),
            ownership: HostOwnership::Managed,
            capacity: HostCapacity {
                ownership: HostOwnership::Managed,
                spec: Some(HostSpec {
                    cpu_milli: 4000,
                    mem_mb: 8192,
                }),
                overcommit_cpu_pct: 100,
                overcommit_mem_pct: 100,
                reserved: ReservedTotals::default(),
                measured: None,
            },
            ready: true,
            failed: false,
            draining: false,
            oci_runtime: true,
            rootless: true,
            arch: Some(Arch::Amd64),
            tenant_zones: BTreeSet::new(),
            tenants: 0,
            has_dedicated_tenant: false,
        }
    }

    fn req() -> PlacementRequest {
        PlacementRequest {
            sandbox: "sz-x".into(),
            worktree: "/wt/x".into(),
            req: ResourceReq {
                cpu_floor_milli: 1000,
                mem_floor_mb: 2048,
                cpu_ceiling_milli: None,
                mem_ceiling_mb: None,
            },
            mode: PlacementModePref::Auto,
            zone: String::new(),
            sealed: false,
            arch: None,
        }
    }

    fn inputs(hosts: Vec<HostSnapshot>) -> PlacementInputs {
        PlacementInputs {
            hosts,
            pack_strategy: PackStrategy::BinPack,
            on_exhaustion: OnExhaustion::Queue,
            autoscale: AutoscaleConfig::default(),
            autoscale_state: AutoscaleSnapshot::default(),
        }
    }

    // ── pack_eligible: flip exactly one field per case ─────────────────────

    #[test]
    fn pack_eligible_reason_matrix() {
        let r = req();
        assert_eq!(pack_eligible(&snap("ok"), &r), Ok(()));

        let mut failed = snap("f");
        failed.failed = true;
        assert_eq!(pack_eligible(&failed, &r), Err(Ineligible::NotReady));

        let mut draining = snap("d");
        draining.draining = true;
        assert_eq!(pack_eligible(&draining, &r), Err(Ineligible::Draining));

        let mut no_oci = snap("n");
        no_oci.oci_runtime = false;
        assert_eq!(pack_eligible(&no_oci, &r), Err(Ineligible::TrustClass));

        let mut rootful = snap("rf");
        rootful.rootless = false;
        assert_eq!(pack_eligible(&rootful, &r), Ok(()), "unsealed: rootful ok");
        let mut sealed = r.clone();
        sealed.sealed = true;
        assert_eq!(
            pack_eligible(&rootful, &sealed),
            Err(Ineligible::TrustClass)
        );

        let mut ded = snap("dd");
        ded.has_dedicated_tenant = true;
        ded.tenants = 1;
        assert_eq!(pack_eligible(&ded, &r), Err(Ineligible::DedicatedOccupied));

        let mut zoned = snap("z");
        zoned.tenant_zones = BTreeSet::from(["clientB".to_string()]);
        zoned.tenants = 1;
        assert_eq!(pack_eligible(&zoned, &r), Err(Ineligible::ZoneCoTenancy));
        // Same zone is fine.
        let mut rz = r.clone();
        rz.zone = "clientB".into();
        assert_eq!(pack_eligible(&zoned, &rz), Ok(()));

        let mut speclesss = snap("s");
        speclesss.capacity.spec = None;
        assert_eq!(pack_eligible(&speclesss, &r), Err(Ineligible::UnknownSpec));

        let mut full = snap("fu");
        full.capacity.reserved = ReservedTotals {
            cpu_milli: 3500,
            mem_mb: 8000,
            tenants: 3,
        };
        assert_eq!(pack_eligible(&full, &r), Err(Ineligible::NoCapacity));

        let mut wrong_arch = snap("wa");
        wrong_arch.arch = Some(Arch::Arm64);
        let mut ra = r.clone();
        ra.arch = Some(Arch::Amd64);
        assert_eq!(pack_eligible(&wrong_arch, &ra), Err(Ineligible::WrongArch));
        // Unknown host arch: eligible (probe confirms at ensure_ready).
        let mut no_arch = snap("na");
        no_arch.arch = None;
        assert_eq!(pack_eligible(&no_arch, &ra), Ok(()));
    }

    #[test]
    fn trust_is_checked_before_capacity() {
        // Empty-but-untrusted reads TrustClass, full-but-trusted NoCapacity —
        // the reasons must not swap.
        let r = req();
        let mut untrusted_empty = snap("u");
        untrusted_empty.oci_runtime = false;
        assert_eq!(
            pack_eligible(&untrusted_empty, &r),
            Err(Ineligible::TrustClass)
        );
        let mut zoned_full = snap("zf");
        zoned_full.tenant_zones = BTreeSet::from(["other".to_string()]);
        zoned_full.capacity.reserved.cpu_milli = 4000;
        assert_eq!(
            pack_eligible(&zoned_full, &r),
            Err(Ineligible::ZoneCoTenancy)
        );
    }

    #[test]
    fn dedicated_eligibility_requires_empty() {
        let r = req();
        assert_eq!(dedicated_eligible(&snap("e"), &r), Ok(()));
        let mut occupied = snap("o");
        occupied.tenants = 1;
        assert_eq!(dedicated_eligible(&occupied, &r), Err(Ineligible::Occupied));
        // No OCI runtime is fine for dedicated (exclusive host, no co-tenancy).
        let mut no_oci = snap("n");
        no_oci.oci_runtime = false;
        assert_eq!(dedicated_eligible(&no_oci, &r), Ok(()));
    }

    // ── ranking ─────────────────────────────────────────────────────────────

    fn with_util(name: &str, cpu_reserved: u64) -> HostSnapshot {
        let mut h = snap(name);
        h.capacity.reserved.cpu_milli = cpu_reserved;
        h.tenants = 1;
        h.tenant_zones = BTreeSet::from([String::new()]);
        h
    }

    #[test]
    fn rank_binpack_most_utilized_first_spread_least() {
        let a = with_util("a", 1000); // 25%
        let b = with_util("b", 3000); // 75%
        let c = with_util("c", 2000); // 50%
        let packed = rank_hosts([&a, &b, &c], PackStrategy::BinPack);
        assert_eq!(
            packed,
            vec![HostId::named("b"), HostId::named("c"), HostId::named("a")]
        );
        let spread = rank_hosts([&a, &b, &c], PackStrategy::Spread);
        assert_eq!(
            spread,
            vec![HostId::named("a"), HostId::named("c"), HostId::named("b")]
        );
    }

    #[test]
    fn rank_prefers_ready_then_ties_on_id() {
        let mut cold = with_util("aaa", 3000);
        cold.ready = false;
        let warm = with_util("zzz", 3000);
        let ranked = rank_hosts([&cold, &warm], PackStrategy::BinPack);
        assert_eq!(ranked[0], HostId::named("zzz"), "ready wins over id order");
        // Equal everything ⇒ id order (total, deterministic).
        let x = with_util("x", 1000);
        let y = with_util("y", 1000);
        assert_eq!(
            rank_hosts([&y, &x], PackStrategy::Spread),
            vec![HostId::named("x"), HostId::named("y")]
        );
    }

    // ── decide_placement ────────────────────────────────────────────────────

    #[test]
    fn auto_packs_when_possible_with_ranked_alternates() {
        let a = with_util("a", 1000);
        let b = with_util("b", 3000);
        let out = decide_placement(&req(), &inputs(vec![a, b]), 100, 100);
        let PlacementDecision::Packed { host, alternates } = out.decision else {
            panic!("expected Packed, got {:?}", out.decision);
        };
        assert_eq!(host, HostId::named("b"), "bin-pack: most utilized");
        assert_eq!(alternates, vec![HostId::named("a")]);
        assert!(out.candidates.iter().all(|(_, why)| why.is_none()));
    }

    #[test]
    fn auto_falls_back_to_dedicated_on_empty_untrusted_host() {
        // No OCI runtime ⇒ not packable, but empty ⇒ dedicated-usable.
        let mut h = snap("bare");
        h.oci_runtime = false;
        let out = decide_placement(&req(), &inputs(vec![h]), 100, 100);
        assert!(
            matches!(out.decision, PlacementDecision::Dedicated { ref host, .. } if *host == HostId::named("bare")),
            "{:?}",
            out.decision
        );
        // The pack-lane trace still recorded WHY it wasn't packed.
        assert_eq!(out.candidates[0].1, Some(Ineligible::TrustClass));
    }

    #[test]
    fn dedicated_mode_takes_smallest_ready_empty_host() {
        let big = {
            let mut h = snap("big");
            h.capacity.spec = Some(HostSpec {
                cpu_milli: 16000,
                mem_mb: 32768,
            });
            h
        };
        let small = snap("small");
        let occupied = with_util("busy", 1000);
        let mut r = req();
        r.mode = PlacementModePref::Dedicated;
        let out = decide_placement(&r, &inputs(vec![big, small, occupied]), 100, 100);
        let PlacementDecision::Dedicated { host, alternates } = out.decision else {
            panic!("{:?}", out.decision);
        };
        // Both empties are 0% utilized ⇒ id order tie-break.
        assert_eq!(host, HostId::named("big"));
        assert_eq!(alternates, vec![HostId::named("small")]);
        assert!(
            out.candidates
                .iter()
                .any(|(h, w)| *h == HostId::named("busy") && *w == Some(Ineligible::Occupied))
        );
    }

    #[test]
    fn packed_mode_never_takes_a_dedicated_fallback() {
        let mut bare = snap("bare");
        bare.oci_runtime = false;
        let mut r = req();
        r.mode = PlacementModePref::Packed;
        let out = decide_placement(&r, &inputs(vec![bare]), 100, 100);
        assert_eq!(out.decision, PlacementDecision::Queue);
    }

    #[test]
    fn exhaustion_policy_maps_queue_reject_halt() {
        for (pol, want) in [
            (OnExhaustion::Queue, PlacementDecision::Queue),
            (OnExhaustion::Reject, PlacementDecision::Reject),
            (OnExhaustion::Error, PlacementDecision::Halt),
        ] {
            let mut i = inputs(vec![]);
            i.on_exhaustion = pol;
            assert_eq!(decide_placement(&req(), &i, 100, 100).decision, want);
        }
    }

    fn lane(provider: &str, size: &str, cpu: &str, mem: &str, max: u32) -> ManagedTemplate {
        ManagedTemplate {
            provider: provider.into(),
            size: size.into(),
            cpu: cpu.into(),
            memory: mem.into(),
            max,
            ..Default::default()
        }
    }

    #[test]
    fn autoscale_walks_lanes_in_order_skipping_cooling_and_capped() {
        let mut i = inputs(vec![]);
        i.autoscale.enabled = true;
        i.autoscale.managed = vec![
            lane("hetzner", "cx22", "2", "4g", 1),
            lane("hetzner", "cx32", "4", "8g", 2),
        ];
        // First lane wins when open.
        let out = decide_placement(&req(), &i, 100, 100);
        let PlacementDecision::Provision { template } = out.decision else {
            panic!("{:?}", out.decision);
        };
        assert_eq!(template.size, "cx22");

        // Cooling first lane ⇒ second.
        i.autoscale_state.cooling = BTreeSet::from(["tpl:hetzner/cx22".to_string()]);
        let out = decide_placement(&req(), &i, 100, 100);
        assert!(
            matches!(out.decision, PlacementDecision::Provision { ref template } if template.size == "cx32")
        );

        // First lane at its per-lane cap ⇒ second.
        i.autoscale_state.cooling.clear();
        i.autoscale_state
            .lane_counts
            .insert("tpl:hetzner/cx22".into(), 1);
        let out = decide_placement(&req(), &i, 100, 100);
        assert!(
            matches!(out.decision, PlacementDecision::Provision { ref template } if template.size == "cx32")
        );

        // Global ceiling stops everything.
        i.autoscale_state.managed_count = 3; // default effective_max_hosts() = 3
        assert_eq!(
            decide_placement(&req(), &i, 100, 100).decision,
            PlacementDecision::Queue
        );
    }

    #[test]
    fn autoscale_skips_lanes_too_small_for_the_ask() {
        let mut i = inputs(vec![]);
        i.autoscale.enabled = true;
        i.autoscale.managed = vec![
            lane("hetzner", "cx22", "2", "4g", 1), // 2 cores < 3-core ask
            lane("hetzner", "cx42", "8", "16g", 1),
        ];
        let mut r = req();
        r.req.cpu_floor_milli = 3000;
        let out = decide_placement(&r, &i, 100, 100);
        assert!(
            matches!(out.decision, PlacementDecision::Provision { ref template } if template.size == "cx42")
        );
        // Overcommit widens what a lane can serve: 200% makes cx22's 2 cores
        // a 4-core ceiling.
        let out = decide_placement(&r, &i, 200, 100);
        assert!(
            matches!(out.decision, PlacementDecision::Provision { ref template } if template.size == "cx22")
        );
        // A spec-less lane can never serve.
        i.autoscale.managed = vec![lane("hetzner", "mystery", "", "", 1)];
        assert_eq!(
            decide_placement(&r, &i, 100, 100).decision,
            PlacementDecision::Queue
        );
    }

    #[test]
    fn autoscale_disabled_goes_straight_to_exhaustion() {
        let mut i = inputs(vec![]);
        i.autoscale.enabled = false;
        i.autoscale.managed = vec![lane("hetzner", "cx22", "2", "4g", 1)];
        assert_eq!(
            decide_placement(&req(), &i, 100, 100).decision,
            PlacementDecision::Queue
        );
    }

    #[test]
    fn decision_is_deterministic() {
        let i = inputs(vec![with_util("a", 1000), with_util("b", 3000), snap("c")]);
        let r = req();
        let one = decide_placement(&r, &i, 200, 100);
        let two = decide_placement(&r, &i, 200, 100);
        assert_eq!(one, two);
    }

    #[test]
    fn decision_tags_cover_every_arm() {
        assert_eq!(
            PlacementDecision::Packed {
                host: HostId::named("h"),
                alternates: vec![]
            }
            .tag(),
            "packed"
        );
        assert_eq!(
            PlacementDecision::Dedicated {
                host: HostId::named("h"),
                alternates: vec![]
            }
            .tag(),
            "dedicated"
        );
        assert_eq!(
            PlacementDecision::Provision {
                template: lane("h", "s", "1", "1g", 1)
            }
            .tag(),
            "provision"
        );
        assert_eq!(PlacementDecision::Queue.tag(), "queued");
        assert_eq!(PlacementDecision::Reject.tag(), "rejected");
        assert_eq!(PlacementDecision::Halt.tag(), "error");
    }

    // ── scale-down ──────────────────────────────────────────────────────────

    fn sd(
        name: &str,
        ownership: HostOwnership,
        tenants: u32,
        idle: u64,
        engine: bool,
    ) -> ScaleDownHost {
        ScaleDownHost {
            host: HostId::named(name),
            ownership,
            tenants,
            engine_created: engine,
            idle_secs: idle,
        }
    }

    #[test]
    fn scaledown_kills_only_idle_empty_engine_managed() {
        let hosts = vec![
            sd("idle-managed", HostOwnership::Managed, 0, 2000, true),
            sd("busy-managed", HostOwnership::Managed, 1, 2000, true),
            sd("fresh-managed", HostOwnership::Managed, 0, 100, true),
            sd(
                "idle-independent",
                HostOwnership::Independent,
                0,
                9999,
                false,
            ),
            sd("user-managed-host", HostOwnership::Managed, 0, 9999, false),
        ];
        let victims = decide_scaledown(&hosts, 900, 0);
        assert_eq!(victims, vec![HostId::named("idle-managed")]);
    }

    #[test]
    fn scaledown_respects_min_hosts_longest_idle_first() {
        let hosts = vec![
            sd("a", HostOwnership::Managed, 0, 1000, true),
            sd("b", HostOwnership::Managed, 0, 3000, true),
            sd("c", HostOwnership::Managed, 0, 2000, true),
        ];
        // min 2 ⇒ only one may die: the longest-idle (b).
        assert_eq!(decide_scaledown(&hosts, 900, 2), vec![HostId::named("b")]);
        // min 0 ⇒ all three, longest-idle first.
        assert_eq!(
            decide_scaledown(&hosts, 900, 0),
            vec![HostId::named("b"), HostId::named("c"), HostId::named("a")]
        );
        // min ≥ total ⇒ nobody dies.
        assert!(decide_scaledown(&hosts, 900, 3).is_empty());
    }

    #[test]
    fn scaledown_pool_spare_holds_the_host() {
        // A ParkIdle spare holds a live tenancy row ⇒ tenants > 0 ⇒ kept warm.
        let hosts = vec![sd("warm", HostOwnership::Managed, 1, 99999, true)];
        assert!(decide_scaledown(&hosts, 900, 0).is_empty());
    }
}
