//! `[placement]` — the placement engine's config surface: whether the broker
//! runs at all, the requested placement class and its constraint floor, the
//! packing strategy, overcommit ratios, per-env resource declarations, and the
//! autoscale template lanes. Extracted as a `config.rs` sibling (the pinned
//! file carries only the `Config.placement` field + re-exports).
//!
//! Key classes follow the config-resolution doctrine
//! ([`crate::config_resolve`]): `mode`/`pack_strategy`/`on_exhaustion` are
//! preferences (most-specific wins); `strictest_allowed_mode`, `overcommit`,
//! and the autoscale table are constraints owned by trusted layers — the repo
//! overlay schema carries none of them, so a `.thegn.*` file structurally
//! cannot loosen fleet policy (the `[host.*]` precedent).

use serde::{Deserialize, Serialize};

use crate::capacity::{HostSpec, ResourceReq, overcommit_pct, parse_cpu_milli, parse_mem_mb};
use crate::config::{Config, EnvConfig, config_enum, config_warn};

config_enum! {
    /// `placement.mode` / `[env.<n>] placement_mode` — the *requested*
    /// placement class. `auto` lets the broker choose (packed when trust and
    /// capacity allow, dedicated otherwise); clamped by the resolved
    /// `strictest_allowed_mode` floor as the final resolution step.
    pub enum PlacementModePref: "placement mode" {
        Auto = "auto",
        Packed = "packed" | "pack",
        Dedicated = "dedicated" | "exclusive",
    } default = Auto;
}

config_enum! {
    /// `placement.pack_strategy` — how eligible hosts are ranked for packing:
    /// `bin-pack` consolidates (most-utilized first, lets idle hosts drain and
    /// scale down); `spread` load-balances (least-utilized first, smaller
    /// blast radius / latency headroom).
    pub enum PackStrategy: "pack strategy" {
        BinPack = "bin-pack" | "bin_pack" | "binpack",
        Spread = "spread",
    } default = BinPack;
}

config_enum! {
    /// `placement.preset` — a named bundle of PREFERENCE defaults (never
    /// constraints). Expansion fills only keys still at their built-in
    /// defaults, so any explicitly-set key wins; everything still flows
    /// through zone clamps + the mode floor.
    pub enum PlacementPreset: "placement preset" {
        None = "none" | "",
        Balanced = "balanced",
        CostOptimized = "cost_optimized" | "cost-optimized",
        LatencyOptimized = "latency_optimized" | "latency-optimized",
        Isolated = "isolated",
    } default = None;
}

config_enum! {
    /// `placement.on_exhaustion` — what happens when every lane is exhausted:
    /// `queue` falls through once and nudges when capacity frees (re-open to
    /// place), `reject` silently falls back to the env's non-engine path,
    /// `error` halts loudly.
    pub enum OnExhaustion: "on exhaustion" {
        Queue = "queue" | "wait",
        Reject = "reject" | "fallback",
        Error = "error" | "halt",
    } default = Queue;
}

/// The strictness lattice for the mode floor (higher = stricter): a floor of
/// `dedicated` collapses `auto`'s choice set to dedicated-only.
fn mode_rank(m: PlacementModePref) -> u8 {
    match m {
        PlacementModePref::Auto => 0,
        PlacementModePref::Packed => 1,
        PlacementModePref::Dedicated => 2,
    }
}

/// Clamp a requested mode by a constraint floor: the request may be stricter,
/// never looser.
pub fn clamp_mode(pref: PlacementModePref, floor: PlacementModePref) -> PlacementModePref {
    if mode_rank(pref) < mode_rank(floor) {
        floor
    } else {
        pref
    }
}

/// A declared resource ask (`[env.<n>.resources]` /
/// `[placement.default_resources]`). String quantities share the container
/// `--cpus`/`--memory` grammar (`"2"`, `"0.5"`, `"500m"` cpus; `"4g"`,
/// `"512m"`, bare-MiB memory); empty = unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ResourcesDecl {
    /// CPU floor the scheduler reserves.
    pub cpu: String,
    /// Memory floor the scheduler reserves.
    pub memory: String,
    /// Optional cpu ceiling — feeds the container `--cpus` limit when the
    /// resolved sandbox declares none of its own. Not part of the fits math.
    pub cpu_max: String,
    /// Optional memory ceiling (same contract as `cpu_max`).
    pub memory_max: String,
}

impl ResourcesDecl {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_empty()
            && self.memory.is_empty()
            && self.cpu_max.is_empty()
            && self.memory_max.is_empty()
    }

    /// Lower to the integer [`ResourceReq`], falling back per-field to
    /// `fallback` (the `[placement.default_resources]` lowering, itself backed
    /// by [`ResourceReq::default`]). Unparseable values warn and fall back —
    /// a typo'd floor must never silently become "no reservation".
    pub fn to_req(&self, fallback: &ResourceReq, ctx: &str) -> ResourceReq {
        let parse = |raw: &str, what: &str, parsed: Option<u64>, fb: u64| -> u64 {
            if raw.trim().is_empty() {
                return fb;
            }
            match parsed {
                Some(v) => v,
                None => {
                    config_warn(&format!("{ctx} {what}: unparseable quantity {raw:?}"));
                    fb
                }
            }
        };
        let cpu = parse(
            &self.cpu,
            "cpu",
            parse_cpu_milli(&self.cpu).map(u64::from),
            u64::from(fallback.cpu_floor_milli),
        ) as u32;
        let mem = parse(
            &self.memory,
            "memory",
            parse_mem_mb(&self.memory),
            fallback.mem_floor_mb,
        );
        let opt = |raw: &str, what: &str, parsed: Option<u64>, fb: Option<u64>| -> Option<u64> {
            if raw.trim().is_empty() {
                return fb;
            }
            match parsed {
                Some(v) => Some(v),
                None => {
                    config_warn(&format!("{ctx} {what}: unparseable quantity {raw:?}"));
                    fb
                }
            }
        };
        ResourceReq {
            cpu_floor_milli: cpu,
            mem_floor_mb: mem,
            cpu_ceiling_milli: opt(
                &self.cpu_max,
                "cpu_max",
                parse_cpu_milli(&self.cpu_max).map(u64::from),
                fallback.cpu_ceiling_milli.map(u64::from),
            )
            .map(|v| v as u32),
            mem_ceiling_mb: opt(
                &self.memory_max,
                "memory_max",
                parse_mem_mb(&self.memory_max),
                fallback.mem_ceiling_mb,
            ),
        }
    }
}

/// One `[[placement.autoscale.managed]]` entry — an ordered failover lane the
/// engine may create a Managed host from when the pool is exhausted.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ManagedTemplate {
    /// VPS provider kind (`"hetzner"` today — must be create-sizable; cloud
    /// sandbox providers are spillover, not managed hosts).
    pub provider: String,
    /// Vendor region/location (empty ⇒ provider default).
    pub region: String,
    /// Vendor size/plan (e.g. Hetzner `cx32`). Required — it IS the spec.
    pub size: String,
    /// Declared machine size of this plan — authoritative for the capacity
    /// index from create time.
    pub cpu: String,
    pub memory: String,
    /// Max concurrent hosts from this lane (`0` ⇒ 1).
    pub max: u32,
}

impl ManagedTemplate {
    /// The lane's declared spec; `None` (with a warn) when cpu/memory are
    /// missing or unparseable — a spec-less lane can never satisfy `fits`.
    pub fn spec(&self) -> Option<HostSpec> {
        let cpu = parse_cpu_milli(&self.cpu);
        let mem = parse_mem_mb(&self.memory);
        match (cpu, mem) {
            (Some(c), Some(m)) => Some(HostSpec {
                cpu_milli: c,
                mem_mb: m,
            }),
            _ => {
                config_warn(&format!(
                    "[[placement.autoscale.managed]] {}/{}: needs parseable cpu + memory",
                    self.provider, self.size
                ));
                None
            }
        }
    }

    /// Stable lane key for cooldown markers (`tpl:<provider>/<size>`).
    pub fn lane_key(&self) -> String {
        format!("tpl:{}/{}", self.provider.trim(), self.size.trim())
    }

    pub fn effective_max(&self) -> u32 {
        if self.max == 0 { 1 } else { self.max }
    }
}

/// `[placement.autoscale]` — creating (and destroying) Managed hosts on
/// demand.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AutoscaleConfig {
    /// Master switch. Enabling it IS the install/create consent for hosts the
    /// engine provisions — thegn created the box, so `install_runtime`
    /// lowers to `auto` on those hosts.
    pub enabled: bool,
    /// Ceiling on engine-created hosts across all lanes (`0` ⇒ unlimited is
    /// NOT allowed for a paid resource — 0 means the built-in default 3).
    pub max_hosts: u32,
    /// Floor kept alive by scale-down (warm capacity).
    pub min_hosts: u32,
    /// A Managed host with zero tenants for longer than this is destroyed.
    pub scale_down_idle_secs: u64,
    /// Base cooldown after a lane's create failure (escalates per consecutive
    /// failure, capped).
    pub cooldown_secs: u64,
    /// Ordered failover lanes.
    pub managed: Vec<ManagedTemplate>,
}

impl Default for AutoscaleConfig {
    fn default() -> Self {
        AutoscaleConfig {
            enabled: false,
            max_hosts: 0,
            min_hosts: 0,
            scale_down_idle_secs: 900,
            cooldown_secs: 60,
            managed: Vec::new(),
        }
    }
}

impl AutoscaleConfig {
    pub fn effective_max_hosts(&self) -> u32 {
        if self.max_hosts == 0 {
            3
        } else {
            self.max_hosts
        }
    }
}

/// `[placement]` — the engine's global/profile-layer table. Off by default:
/// with `enabled = false` every existing spawn path is byte-identical.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PlacementConfig {
    /// Master switch for the broker. Off ⇒ inert (no decisions, no tenancy).
    pub enabled: bool,
    /// Named preference bundle (see [`PlacementPreset`]).
    pub preset: PlacementPreset,
    /// Requested placement class (preference; `[env.<n>] placement_mode`
    /// overrides per env).
    pub mode: PlacementModePref,
    /// Constraint floor on the mode (trusted layers only): requests may be
    /// stricter, never looser. `auto` = no floor.
    pub strictest_allowed_mode: PlacementModePref,
    pub pack_strategy: PackStrategy,
    pub on_exhaustion: OnExhaustion,
    /// CPU overcommit ratio for packing ceilings (floors are declared asks,
    /// not real usage — 2.0 is conservative for dev workloads).
    pub overcommit: f64,
    /// Memory overcommit ratio (1.0: memory is not compressible).
    pub mem_overcommit: f64,
    /// Fallback resource ask when an env declares none.
    pub default_resources: ResourcesDecl,
    /// Compounding-uncertainty haircut for INDEPENDENT hosts (percent,
    /// clamped 1..=100): their packing ceiling is
    /// `min(declared, probed) × overcommit × this` — an uncontrolled box
    /// carries invisible co-workloads and thegn has no eviction lever.
    pub independent_safety_pct: u32,
    /// Max age of a host's measured headroom sample before a placement
    /// decision refreshes it (lazily — never the idle ticker).
    pub headroom_ttl_secs: u64,
    /// Ordered SPILLOVER lane: `[env.<name>]` entries with a provider
    /// placement, tried (health- and budget-gated) when the owned pool and
    /// autoscale are exhausted. Empty ⇒ no spillover.
    pub spillover_envs: Vec<String>,
    /// `[placement.price]` — hourly USD rates for the compute ledger, keyed
    /// `"<provider>:<size>"` (autoscaled hosts) or `"<provider>"`. Unpriced
    /// resources meter at 0 with a one-time warning.
    pub price: std::collections::BTreeMap<String, f64>,
    /// Monthly compute spend cap in USD for the `global` ledger scope
    /// (`0` ⇒ uncapped). Breach refuses/queues PAID lanes (autoscale,
    /// spillover) while packing onto already-paid hosts keeps serving.
    pub max_monthly_spend: f64,
    pub autoscale: AutoscaleConfig,
}

impl Default for PlacementConfig {
    fn default() -> Self {
        PlacementConfig {
            enabled: false,
            preset: PlacementPreset::None,
            mode: PlacementModePref::Auto,
            strictest_allowed_mode: PlacementModePref::Auto,
            pack_strategy: PackStrategy::BinPack,
            on_exhaustion: OnExhaustion::Queue,
            overcommit: 2.0,
            mem_overcommit: 1.0,
            default_resources: ResourcesDecl::default(),
            independent_safety_pct: 85,
            headroom_ttl_secs: 60,
            spillover_envs: Vec::new(),
            price: std::collections::BTreeMap::new(),
            max_monthly_spend: 0.0,
            autoscale: AutoscaleConfig::default(),
        }
    }
}

impl PlacementPreset {
    /// Expand into the still-at-default preference keys of `pl`. Presets are
    /// structurally unable to touch constraint keys (they only ever assign
    /// the preference fields below).
    pub fn expand_into(self, pl: &mut PlacementConfig) {
        let d = PlacementConfig::default();
        let mut set = |mode: PlacementModePref, pack: PackStrategy, on: OnExhaustion| {
            if pl.mode == d.mode {
                pl.mode = mode;
            }
            if pl.pack_strategy == d.pack_strategy {
                pl.pack_strategy = pack;
            }
            if pl.on_exhaustion == d.on_exhaustion {
                pl.on_exhaustion = on;
            }
        };
        match self {
            PlacementPreset::None | PlacementPreset::Balanced => {}
            PlacementPreset::CostOptimized => set(
                PlacementModePref::Packed,
                PackStrategy::BinPack,
                OnExhaustion::Queue,
            ),
            PlacementPreset::LatencyOptimized => set(
                PlacementModePref::Auto,
                PackStrategy::Spread,
                OnExhaustion::Reject,
            ),
            PlacementPreset::Isolated => set(
                PlacementModePref::Dedicated,
                PackStrategy::Spread,
                OnExhaustion::Error,
            ),
        }
    }
}

/// The fully-resolved placement policy for one spawn — the broker's ONLY
/// config input (it never re-derives trust or re-reads raw layers).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPlacement {
    pub enabled: bool,
    /// The clamped mode (request vs. the max of the global + zone floors).
    pub mode: PlacementModePref,
    pub pack_strategy: PackStrategy,
    pub on_exhaustion: OnExhaustion,
    pub overcommit_cpu_pct: u32,
    pub overcommit_mem_pct: u32,
    pub req: ResourceReq,
    /// The floor collapsed a looser request (surfaced by `placement explain`).
    pub floor_applied: bool,
}

/// Resolve the placement policy for one env: global `[placement]` → env
/// `placement_mode`/`resources` → the terminal mode-floor clamp (the max of
/// `strictest_allowed_mode` and the zone's `placement_floor`). Pure.
pub fn resolve_placement(
    cfg: &Config,
    env: Option<&EnvConfig>,
    zone_floor: Option<PlacementModePref>,
) -> ResolvedPlacement {
    let mut expanded = cfg.placement.clone();
    expanded.preset.expand_into(&mut expanded);
    let pl = &expanded;
    let requested = env.and_then(|e| e.placement_mode).unwrap_or(pl.mode);
    let floor = match zone_floor {
        Some(zf) if mode_rank(zf) > mode_rank(pl.strictest_allowed_mode) => zf,
        _ => pl.strictest_allowed_mode,
    };
    let mode = clamp_mode(requested, floor);
    let defaults = pl
        .default_resources
        .to_req(&ResourceReq::default(), "[placement.default_resources]");
    let req = match env {
        Some(e) if !e.resources.is_empty() => e.resources.to_req(&defaults, "[env.*.resources]"),
        _ => defaults,
    };
    ResolvedPlacement {
        enabled: pl.enabled,
        mode,
        pack_strategy: pl.pack_strategy,
        on_exhaustion: pl.on_exhaustion,
        overcommit_cpu_pct: overcommit_pct(pl.overcommit),
        overcommit_mem_pct: overcommit_pct(pl.mem_overcommit),
        req,
        floor_applied: mode != requested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn defaults_are_off_and_safe() {
        let pl = PlacementConfig::default();
        assert!(!pl.enabled);
        assert_eq!(pl.mode, PlacementModePref::Auto);
        assert_eq!(pl.strictest_allowed_mode, PlacementModePref::Auto);
        assert_eq!(pl.pack_strategy, PackStrategy::BinPack);
        assert_eq!(pl.on_exhaustion, OnExhaustion::Queue);
        assert_eq!(pl.overcommit, 2.0);
        assert_eq!(pl.mem_overcommit, 1.0);
        assert!(!pl.autoscale.enabled);
        assert_eq!(pl.autoscale.effective_max_hosts(), 3);
    }

    #[test]
    fn toml_round_trip_and_aliases() {
        let cfg = cfg_from(
            r#"
            [placement]
            enabled = true
            mode = "pack"
            strictest_allowed_mode = "packed"
            pack_strategy = "bin_pack"
            on_exhaustion = "halt"
            overcommit = 1.5
            [placement.default_resources]
            cpu = "0.5"
            memory = "1g"
            [placement.autoscale]
            enabled = true
            max_hosts = 2
            [[placement.autoscale.managed]]
            provider = "hetzner"
            size = "cx32"
            cpu = "4"
            memory = "8g"
            max = 2
            "#,
        );
        let pl = &cfg.placement;
        assert!(pl.enabled);
        assert_eq!(pl.mode, PlacementModePref::Packed, "alias `pack`");
        assert_eq!(pl.on_exhaustion, OnExhaustion::Error, "alias `halt`");
        assert_eq!(pl.overcommit, 1.5);
        assert_eq!(pl.autoscale.managed.len(), 1);
        let t = &pl.autoscale.managed[0];
        assert_eq!(t.lane_key(), "tpl:hetzner/cx32");
        assert_eq!(
            t.spec(),
            Some(HostSpec {
                cpu_milli: 4000,
                mem_mb: 8192
            })
        );
        assert_eq!(t.effective_max(), 2);
    }

    #[test]
    fn template_without_spec_is_none_and_zero_max_is_one() {
        let t = ManagedTemplate {
            provider: "hetzner".into(),
            size: "cx22".into(),
            ..Default::default()
        };
        assert_eq!(t.spec(), None);
        assert_eq!(t.effective_max(), 1);
        let bad = ManagedTemplate {
            cpu: "lots".into(),
            memory: "8g".into(),
            ..Default::default()
        };
        assert_eq!(bad.spec(), None);
    }

    #[test]
    fn mode_floor_lattice() {
        use PlacementModePref::*;
        // request × floor → clamped
        for (req, floor, want) in [
            (Auto, Auto, Auto),
            (Auto, Packed, Packed),
            (Auto, Dedicated, Dedicated),
            (Packed, Auto, Packed),
            (Packed, Dedicated, Dedicated),
            (Dedicated, Auto, Dedicated),
            (Dedicated, Packed, Dedicated),
            (Dedicated, Dedicated, Dedicated),
        ] {
            assert_eq!(clamp_mode(req, floor), want, "{req:?} under {floor:?}");
        }
    }

    #[test]
    fn resolve_applies_env_pref_and_the_stricter_floor() {
        let cfg = cfg_from(
            r#"
            [placement]
            enabled = true
            mode = "packed"
            strictest_allowed_mode = "packed"
            [env.iso]
            placement_mode = "auto"
            "#,
        );
        // Zone floor (dedicated) is stricter than the global floor (packed).
        let r = resolve_placement(&cfg, cfg.env.get("iso"), Some(PlacementModePref::Dedicated));
        assert_eq!(r.mode, PlacementModePref::Dedicated);
        assert!(r.floor_applied);

        // No zone floor: env `auto` clamps to the global `packed` floor.
        let r2 = resolve_placement(&cfg, cfg.env.get("iso"), None);
        assert_eq!(r2.mode, PlacementModePref::Packed);
        assert!(r2.floor_applied);

        // Env absent ⇒ global mode, already at the floor ⇒ not flagged.
        let r3 = resolve_placement(&cfg, None, None);
        assert_eq!(r3.mode, PlacementModePref::Packed);
        assert!(!r3.floor_applied);
    }

    #[test]
    fn resolve_resources_env_over_default_over_builtin() {
        let cfg = cfg_from(
            r#"
            [placement]
            [placement.default_resources]
            cpu = "0.5"
            memory = "1g"
            [env.big]
            [env.big.resources]
            cpu = "4"
            memory = "8g"
            cpu_max = "8"
            [env.plain]
            "#,
        );
        let big = resolve_placement(&cfg, cfg.env.get("big"), None);
        assert_eq!(big.req.cpu_floor_milli, 4000);
        assert_eq!(big.req.mem_floor_mb, 8192);
        assert_eq!(big.req.cpu_ceiling_milli, Some(8000));
        assert_eq!(big.req.mem_ceiling_mb, None);

        // Env without resources ⇒ the configured defaults.
        let plain = resolve_placement(&cfg, cfg.env.get("plain"), None);
        assert_eq!(plain.req.cpu_floor_milli, 500);
        assert_eq!(plain.req.mem_floor_mb, 1024);

        // No config defaults either ⇒ the builtin 1 core / 2 GiB.
        let bare = cfg_from("");
        let b = resolve_placement(&bare, None, None);
        assert_eq!(b.req, ResourceReq::default());
    }

    #[test]
    fn partial_env_resources_fall_back_per_field() {
        let cfg = cfg_from(
            r#"
            [placement.default_resources]
            cpu = "2"
            memory = "4g"
            [env.memonly]
            [env.memonly.resources]
            memory = "512m"
            "#,
        );
        let r = resolve_placement(&cfg, cfg.env.get("memonly"), None);
        assert_eq!(r.req.cpu_floor_milli, 2000, "cpu falls back to default");
        assert_eq!(r.req.mem_floor_mb, 512);
    }

    #[test]
    fn junk_quantities_warn_and_fall_back() {
        let d = ResourcesDecl {
            cpu: "banana".into(),
            memory: "3g".into(),
            cpu_max: "also-junk".into(),
            memory_max: String::new(),
        };
        let r = d.to_req(&ResourceReq::default(), "test");
        assert_eq!(r.cpu_floor_milli, 1000, "junk cpu ⇒ fallback");
        assert_eq!(r.mem_floor_mb, 3072);
        assert_eq!(r.cpu_ceiling_milli, None, "junk ceiling ⇒ fallback (none)");
    }

    #[test]
    fn overcommit_lowered_to_pct() {
        let mut cfg = cfg_from("[placement]\novercommit = 3.0\nmem_overcommit = 1.5\n");
        let r = resolve_placement(&cfg, None, None);
        assert_eq!(r.overcommit_cpu_pct, 300);
        assert_eq!(r.overcommit_mem_pct, 150);
        cfg.placement.overcommit = 0.25;
        assert_eq!(resolve_placement(&cfg, None, None).overcommit_cpu_pct, 100);
    }

    #[test]
    fn zone_placement_floor_parses() {
        let cfg = cfg_from("[zone.clientA]\nplacement_floor = \"dedicated\"\n");
        assert_eq!(
            cfg.zone["clientA"].placement_floor,
            Some(PlacementModePref::Dedicated)
        );
    }

    #[test]
    fn resources_decl_is_empty() {
        assert!(ResourcesDecl::default().is_empty());
        assert!(
            !ResourcesDecl {
                cpu: "1".into(),
                ..Default::default()
            }
            .is_empty()
        );
    }
}
