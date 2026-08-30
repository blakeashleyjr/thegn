//! `thegn doctor` — report the detected terminal capabilities and the
//! feature degradation that follows from them. The manual test surface for the
//! whole terminal-compatibility layer: it shows the raw environment, what
//! `thegn_core::termcaps::detect` makes of it, the effective `[theme]` modes,
//! and the final resolved capabilities (after config overrides) — so you can
//! confirm what a given terminal gets without launching the compositor.

use anyhow::Result;
use thegn_core::capabilities::{Capabilities, IsolationClass};
use thegn_core::config::{Config, SandboxProfile};
use thegn_core::managed_tool::{ManagedTool, Resolution};
use thegn_core::outln;
use thegn_core::placement::{Placement, RuntimeProbe};
use thegn_core::sandbox::Backend;
use thegn_core::seam::Kind as _;
use thegn_core::store::HostStore;
use thegn_core::termcaps::{ColorDepth, TermCaps, TermEnv, UnicodeLevel};

fn color_str(d: ColorDepth) -> &'static str {
    match d {
        ColorDepth::Truecolor => "truecolor (24-bit)",
        ColorDepth::Ansi256 => "256-color",
        ColorDepth::Ansi16 => "16-color",
        ColorDepth::None => "monochrome (no color)",
    }
}

fn unicode_str(l: UnicodeLevel) -> &'static str {
    match l {
        UnicodeLevel::Full => "full (Unicode + wide glyphs)",
        UnicodeLevel::Basic => "basic (Unicode BMP)",
        UnicodeLevel::Ascii => "ascii (7-bit fallback)",
    }
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// How the outer terminal answered the startup keyboard probe, as the
/// `keyboard` row of `Resolved capabilities`.
///
/// `None` (no probe, or a terminal that stayed silent) is reported as *unknown*
/// and never as broken — thegn assumes the chords work unless the terminal said
/// otherwise. See THE-70.
fn keyboard_str(ctrl_digits: Option<bool>) -> &'static str {
    match ctrl_digits {
        Some(true) => "modifyOtherKeys=2 (Ctrl+<digit> chords OK)",
        Some(false) => "not reported (Ctrl+1..9 / Ctrl+Alt+1..9 cannot reach thegn)",
        None => "unknown (no probe — assuming supported)",
    }
}

/// The actionable remedy printed under a broken `keyboard` row. `in_tmux`
/// selects the multiplexer fix, which is by far the most common cause (tmux
/// swallows `CSI > 4 ; 2 m` unless extended keys are enabled).
fn keyboard_remedy(in_tmux: bool) -> Vec<&'static str> {
    let mut out = Vec::new();
    if in_tmux {
        out.push("inside tmux: set -g extended-keys on");
        out.push("  (tmux 3.4+ also: set -as terminal-features '*:extkeys')");
    } else {
        out.push("use a terminal supporting xterm modifyOtherKeys level 2,");
        out.push("  or rebind in [keybinds]: summon-workspace-1 … -9 and");
        out.push("  summon-pin-1 … -9, e.g. summon-workspace-1 = \"Ctrl Alt q\"");
    }
    out
}

/// The honest boundary class a named backend resolves to at `Local` placement,
/// under the configured OCI runtime (`runsc`/`krun` raise it for OCI backends).
fn isolation_of(backend_name: &str, oci_runtime: Option<&str>) -> Option<IsolationClass> {
    isolation_of_on(
        backend_name,
        oci_runtime,
        thegn_core::sandbox_backend::host_os(),
    )
}

/// [`isolation_of`] with the OS explicit, so the reported class can be tested
/// for every platform from one machine. A local OCI container on macOS runs in
/// a VM, so it reports `guest-kernel` there and `shared-kernel` on Linux.
fn isolation_of_on(
    backend_name: &str,
    oci_runtime: Option<&str>,
    os: thegn_core::sandbox_backend::HostOs,
) -> Option<IsolationClass> {
    let backend = Backend::parse(backend_name)?;
    Some(Capabilities::from_parts_on(backend, &Placement::Local, false, oci_runtime, os).isolation)
}

/// The configured `[sandbox] oci_runtime`, or `None` when unset (daemon default).
fn cfg_oci_runtime(cfg: &Config) -> Option<&str> {
    (!cfg.sandbox.oci_runtime.trim().is_empty()).then_some(cfg.sandbox.oci_runtime.as_str())
}

/// A `" (…)"` availability suffix for the configured OCI runtime: whether its
/// binary (and `/dev/kvm`, for libkrun) is present on THIS host, so the user
/// learns a strong runtime would silently fall back before they rely on it.
fn oci_runtime_status(rt: &str) -> String {
    use thegn_core::sandbox_runtime::{RuntimeDecision, decide, runtime_req};
    let Some(req) = runtime_req(rt.trim()) else {
        return String::new(); // runc/crun/unknown: no extra requirement to report.
    };
    let binary_present = thegn_core::util::which_path(req.binary).is_some();
    let kvm_present = std::path::Path::new("/dev/kvm").exists();
    match decide(Some(rt), binary_present, kvm_present) {
        RuntimeDecision::Keep => " (available)".to_string(),
        RuntimeDecision::Degrade(reason) => format!(" (unavailable — {reason})"),
    }
}

/// A one-line summary of the OS-isolation knobs a hardening preset imposes.
fn profile_policy(p: SandboxProfile) -> String {
    let mut parts = Vec::new();
    if p.forces_no_network() {
        parts.push("network=none".to_string());
    }
    if p.read_only_root() {
        parts.push("read-only root".to_string());
    }
    if p.no_new_privileges() {
        parts.push("no-new-privs".to_string());
    }
    if let Some(n) = p.pids_limit() {
        parts.push(format!("pids\u{2264}{n}"));
    }
    parts.push(if p.drop_capabilities().iter().any(|c| c == "ALL") {
        "caps: drop ALL".to_string()
    } else {
        "caps: runtime default".to_string()
    });
    parts.join(", ")
}

/// The candidate backends doctor reports for the human shell — the concrete
/// configured backend, or the `backend_chain` when `backend = auto`.
fn shell_chain(cfg: &Config) -> Vec<String> {
    if Backend::from_config(cfg.sandbox.backend).is_some() {
        vec![cfg.sandbox.backend.as_str().to_string()]
    } else {
        cfg.sandbox.backend_chain.clone()
    }
}

fn sandbox_json(cfg: &Config) -> serde_json::Value {
    let chain: Vec<serde_json::Value> = shell_chain(cfg)
        .iter()
        .map(|name| {
            serde_json::json!({
                "backend": name,
                "isolation": isolation_of(name, cfg_oci_runtime(cfg)).map(|c| c.as_str()),
            })
        })
        .collect();
    serde_json::json!({
        "enabled": cfg.sandbox.enabled,
        "backend": cfg.sandbox.backend.as_str(),
        "candidates": chain,
        "network": cfg.sandbox.network.as_str(),
        "shell_profile": {
            "name": cfg.sandbox.profile.as_str(),
            "policy": profile_policy(cfg.sandbox.profile),
        },
        "limits": limits_json(cfg),
        "isolation_floor": cfg.sandbox.isolation_floor.as_str(),
        "on_floor_miss": cfg.sandbox.on_floor_miss.as_str(),
        "enforcement_matrix": enforcement_matrix_json(),
        "home": home_json(cfg),
    })
}

/// The derived enforcement matrix for the running host, as JSON — one object per
/// reachable backend with its honest cells. Aggregation-only (see
/// [`enforcement_matrix_report`]).
fn enforcement_matrix_json() -> serde_json::Value {
    let os = thegn_core::sandbox_backend::host_os();
    let probed = thegn_core::sandbox_cpucap::detect_cpu_cap();
    let rows: Vec<serde_json::Value> = thegn_core::sandbox_matrix::column_for(os)
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "backend": r.backend.label(),
                "fs": r.fs.as_str(),
                "net": r.net.as_str(),
                "ceiling": r.ceiling_label(Some(probed)),
                "scoping": r.scoping.as_str(),
                "class": r.class.as_str(),
                "verified": r.verified,
            })
        })
        .collect();
    serde_json::json!({ "host_os": os.as_str(), "rows": rows })
}

/// The resolved CPU/memory caps + enforcement mechanism for `--json`.
fn limits_json(cfg: &Config) -> serde_json::Value {
    let limits = &cfg.sandbox.limits;
    // `physical_ncpu`, not `available_parallelism` — the same source
    // `set_aggregate_caps` publishes from, so doctor reports the number that
    // would actually be written rather than this process's cgroup share.
    let ncpu = thegn_core::sandbox_cpucap::physical_ncpu();
    let live = live_slice_caps();
    serde_json::json!({
        "cpu_per_pane": limits.cpu,
        "cpu_total": thegn_core::sandbox_cpucap::resolve_cpu_total(
            limits.cpu_total.as_deref().unwrap_or("auto"), ncpu),
        "memory": limits.memory,
        "cpu_enforcement": cpu_cap_key(thegn_core::sandbox_cpucap::detect_cpu_cap()),
        "slice_live": live.as_ref().map(|l| serde_json::json!({
            "unit": thegn_core::sandbox_cpucap::CPU_SLICE,
            "cpu_quota": l.cpu_quota,
            "cpu_weight": l.cpu_weight,
            "memory_high": l.memory_high,
        })),
        "nested_instance": thegn_core::sandbox_cpucap::inside_thegn_slice(),
    })
}

/// What the shared `thegn.slice` is carrying **right now**, read back from the
/// user manager. Configured-vs-live is the diagnostic that matters here: the
/// slice is one process-wide object written by `systemctl set-property`, so its
/// live value can differ from this config for entirely legitimate reasons (the
/// user raised it by hand) and for bad ones (an older thegn, or a nested
/// instance, wrote a smaller ceiling). Neither is visible from the config alone.
struct LiveSliceCaps {
    cpu_quota: Option<String>,
    cpu_weight: Option<String>,
    memory_high: Option<String>,
}

/// Read the live slice properties. `None` when there is no systemd user manager,
/// no `systemctl`, or the unit does not exist — best-effort, never a failure:
/// doctor reports what it can observe and stays silent about what it cannot.
// off-loop: doctor is a synchronous CLI verb
#[expect(clippy::disallowed_methods)]
fn live_slice_caps() -> Option<LiveSliceCaps> {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            thegn_core::sandbox_cpucap::CPU_SLICE,
            "-p",
            "CPUQuotaPerSecUSec",
            "-p",
            "CPUWeight",
            "-p",
            "MemoryHigh",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |key: &str| -> Option<String> {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty() && v != "[not set]")
    };
    // `infinity` means "nothing set here", which is a real answer worth showing
    // as "unset" rather than as a value. The byte count is re-spelled the way
    // `[sandbox.limits]` spells one, so the live line and the configured line
    // are directly comparable by eye as well as by value.
    let memory_high = get("MemoryHigh")
        .as_deref()
        .and_then(thegn_core::sandbox_cpucap::mem_bytes)
        .map(thegn_core::sandbox_cpucap::format_mem_bytes);
    Some(LiveSliceCaps {
        cpu_quota: get("CPUQuotaPerSecUSec")
            .as_deref()
            .and_then(thegn_core::sandbox_cpucap::quota_usec_to_percent),
        cpu_weight: get("CPUWeight"),
        memory_high,
    })
}

/// The personal-shell-layer summary for `--json`: the global strategy and any
/// per-env strategy overrides.
fn home_json(cfg: &Config) -> serde_json::Value {
    let mut envs: Vec<(&String, &str)> = cfg
        .env
        .iter()
        .filter_map(|(n, e)| {
            e.sandbox
                .home
                .as_ref()
                .and_then(|h| h.strategy)
                .map(|s| (n, s.as_str()))
        })
        .collect();
    envs.sort_by(|a, b| a.0.cmp(b.0));
    let env_overrides: serde_json::Map<String, serde_json::Value> = envs
        .into_iter()
        .map(|(n, s)| (n.clone(), serde_json::Value::from(s)))
        .collect();
    serde_json::json!({
        "strategy": cfg.sandbox.home.strategy.as_str(),
        "portable_dotfiles_only": cfg.sandbox.home.portable_dotfiles_only,
        "env_overrides": env_overrides,
    })
}

/// Every provider the loaded config selects, with its probe (the
/// provider-seams registry). Reserved kinds show up as unavailable with the
/// reason, so "why is CI empty?" has a one-line answer.
fn providers_json(cfg: &Config) -> serde_json::Value {
    serde_json::to_value(thegn_svc::seam::registry::probes(cfg)).unwrap_or_default()
}

/// Text twin of [`providers_json`]. Never affects the exit status: a missing
/// optional binary is information, not a doctor failure.
fn providers_report(cfg: &Config) {
    use thegn_core::seam::Availability;
    outln!("Providers (seam → provider: availability)");
    for r in thegn_svc::seam::registry::probes(cfg) {
        let (state, why) = match &r.availability {
            Availability::Ready => ("ready", String::new()),
            Availability::Degraded(w) => ("degraded", format!(" — {w}")),
            Availability::Unavailable(w) => ("unavailable", format!(" — {w}")),
        };
        outln!("  {:<9} {:<24} {state}{why}", r.seam, r.id);
        for n in &r.notes {
            outln!("  {:<9} {:<24}   {n}", "", "");
        }
    }
}

/// The `Secrets` section: one probe row per backend kind (keyring/file/env,
/// with `exec` reserved), then one presence-only line per configured secret
/// field — its backend and `resolves`/`missing`, never a value (THE-66).
fn secrets_report(cfg: &Config) {
    use thegn_core::seam::Availability;
    outln!("Secrets (broker backend → availability)");
    for r in crate::secret::probes() {
        let (state, why) = match &r.availability {
            Availability::Ready => ("ready", String::new()),
            Availability::Degraded(w) => ("degraded", format!(" — {w}")),
            Availability::Unavailable(w) => ("unavailable", format!(" — {w}")),
        };
        outln!("  {:<9} {state}{why}", r.id);
    }
    let refs = thegn_core::secret_scan::secret_refs(cfg);
    if refs.is_empty() {
        outln!("  (no secret refs configured)");
        return;
    }
    outln!("  configured refs (field → backend: presence, never a value):");
    for f in &refs {
        let present = if crate::secret::present(&f.reference) {
            "resolves"
        } else {
            "missing"
        };
        outln!(
            "  {:<44} {:<8} {}",
            f.path,
            f.reference.backend_kind(),
            present
        );
    }
}

/// The host-key policy table: the four connection classes → policy →
/// justification (THE-66). One checkable place for "what host-key posture does
/// each kind of ssh connection get, and why".
fn hostkey_report() {
    use thegn_core::hostkey::HostKeyClass;
    outln!("SSH host-key policy (class → policy — justification)");
    for class in HostKeyClass::ALL {
        outln!("  {:<18} {}", class.label(), class.policy_summary());
        outln!("  {:<18}   ({})", "", class.justification());
    }
}

/// The per-tier secret-exposure listing: exactly which secret-bearing env vars,
/// sockets, and mounts each sandbox tier's effective config would hand a pane
/// (THE-66). Makes the trade visible before it bites.
fn exposure_report(cfg: &Config) {
    use thegn_core::config::SandboxProfile;
    outln!("Sandbox secret exposure (per tier, from the effective [sandbox] config)");
    let pass = &cfg.sandbox.env_passthrough;
    let mounts = &cfg.sandbox.mounts;
    // Secret-bearing env vars named in the passthrough (this shows what the
    // config WOULD expose if the var is set in the launching env).
    let secret_env: Vec<&str> = pass
        .iter()
        .map(String::as_str)
        .filter(|k| {
            thegn_core::redact::is_sensitive(k)
                || matches!(*k, "SSH_AUTH_SOCK" | "GH_TOKEN" | "GITHUB_TOKEN")
        })
        .collect();
    let bus_mount = mounts.iter().any(|m| m.starts_with("/run/user"));
    let gpg_mount = mounts.iter().find(|m| m.contains(".gnupg"));
    for tier in [
        SandboxProfile::Open,
        SandboxProfile::Hardened,
        SandboxProfile::Sealed,
        SandboxProfile::SealedTunnel,
    ] {
        outln!("  [{}]", tier);
        if matches!(tier, SandboxProfile::Open) {
            outln!(
                "    (no sandbox — a pane has the full user session: dotfiles, ~/.ssh, keyring)"
            );
            continue;
        }
        // The sealed tiers clamp the agent socket regardless of the passthrough.
        let seals = tier.seals_agent_socket();
        let names: Vec<&str> = secret_env
            .iter()
            .copied()
            .filter(|k| !(seals && *k == "SSH_AUTH_SOCK"))
            .collect();
        if names.is_empty() {
            outln!("    env         (none of the secret-bearing passthrough vars)");
        } else {
            outln!("    env         {}", names.join(", "));
        }
        outln!(
            "    agent sock  {}",
            if seals {
                "sealed (SSH_AUTH_SOCK dropped; /run/user not mounted)"
            } else if bus_mount {
                "reachable (SSH_AUTH_SOCK + /run/user mounted)"
            } else if pass.iter().any(|k| k == "SSH_AUTH_SOCK") {
                "SSH_AUTH_SOCK passed, but /run/user not mounted (socket unreachable)"
            } else {
                "not exposed"
            }
        );
        match gpg_mount {
            Some(m) => outln!("    gpg home    {m}"),
            None => outln!("    gpg home    (not mounted)"),
        }
    }
    outln!("  note: `/run/user` is not a default mount (keyring/agent unreachable);");
    outln!("        add it to [sandbox] mounts only if a pane needs the session bus.");
}

/// One-line control-surface coverage summary (cells implemented / declared,
/// gap count). The full per-surface ledger is `thegn api coverage`.
fn control_surface_report() {
    let ledgers = crate::cmd::api::surface_ledgers();
    let implemented: usize = ledgers.iter().map(|l| l.implemented + l.stub).sum();
    let declared: usize = ledgers.iter().map(|l| l.declared).sum();
    let stubs: usize = ledgers.iter().map(|l| l.stub).sum();
    let gaps: usize = ledgers.iter().map(|l| l.excused).sum();
    outln!("Control-surface coverage (see `thegn api coverage`)");
    outln!(
        "  cells         {implemented}/{declared} implemented ({stubs} stub, {gaps} excused gap{})",
        if gaps == 1 { "" } else { "s" }
    );
}

/// Where shell completions are installed for `thegn` and `tg`, and whether they
/// are current — the detection half of the completions story (a package
/// regenerates the file with the binary, but a hand-installed one can drift and
/// nothing else would ever say so). Logic lives in
/// [`crate::completions_health`]; this only renders it.
///
/// A healthy install collapses to one line: only rows that need the user to do
/// something are worth scrolling, and those carry the exact fix command. Either
/// way it closes with [`value_source_report`] — the seam's own projection.
fn completions_report() {
    use crate::completions_health::State;
    let report = crate::completions_health::report();
    outln!("Completions");
    if !report.needs_attention() {
        let dynamic = report
            .rows
            .iter()
            .filter(|r| r.state == State::Dynamic)
            .count();
        outln!(
            "  {} installed and current ({dynamic} dynamic shim{}, which never go stale)",
            report.rows.len(),
            if dynamic == 1 { "" } else { "s" }
        );
        value_source_report();
        return;
    }
    for row in &report.rows {
        let path = row.path.display();
        let detail = match row.state {
            State::Fresh => format!("{path}"),
            State::Dynamic => format!("{path}  (shim — asks the binary, never stale)"),
            State::Stale => format!("{path} — run: {}", row.fix),
            State::Absent => format!("— run: {}", row.fix),
        };
        outln!(
            "  {:<6} {:<6} {:<8} {detail}",
            row.shell.as_str(),
            row.command,
            row.state.as_str()
        );
    }
    value_source_report();
}

/// The value-source seam's projection into doctor: how many kinds a `<TAB>`
/// serves, and every kind that is `reserved` with the reason it is not served.
///
/// This is the third leg of the seam idiom (`docs/ARCHITECTURE.md` §5 — trait,
/// implemented-or-`reserved` kind, probe). Without it the reasons live in
/// `thegn_core::completion::SourceKind` where only a reader of the source can
/// find them, and "branch names do not complete" reads as a bug rather than as
/// the fast-path contract holding. Describes the build, not the machine, so it
/// is the same handful of lines on every run.
fn value_source_report() {
    use thegn_core::completion::SourceKind;
    let live = SourceKind::ALL
        .iter()
        .filter(|k| k.is_implemented())
        .count();
    // Reserved kinds share reasons (pr and issue are both "network"), so group
    // by reason rather than repeating it.
    let mut groups: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    for kind in SourceKind::ALL {
        let Some(reason) = kind.reserved_reason() else {
            continue;
        };
        match groups.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, kinds)) => kinds.push(kind.kind()),
            None => groups.push((reason, vec![kind.kind()])),
        }
    }
    let reserved: usize = groups.iter().map(|(_, k)| k.len()).sum();
    outln!("  value sources  {live} live, {reserved} reserved");
    for (reason, kinds) in &groups {
        // A colon, not the "— reason" dash the provider rows use: two of these
        // reasons already contain a dash, and "reserved — network — a <TAB> …"
        // reads as a stutter.
        outln!("    {:<16} reserved: {reason}", kinds.join(", "));
    }
}

/// Text twin of [`completions_report`] for `--json`: one object per
/// (shell, command) pair.
fn completions_json() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = crate::completions_health::report()
        .rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "shell": r.shell.as_str(),
                "command": r.command,
                "state": r.state.as_str(),
                "path": r.path.display().to_string(),
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

/// The release channel + per-feature allow table for `--json`.
fn channel_json() -> serde_json::Value {
    let channel = crate::channel_state::current();
    let features: serde_json::Map<String, serde_json::Value> = thegn_core::channel::Feature::ALL
        .iter()
        .map(|f| {
            (
                f.id().to_string(),
                serde_json::Value::from(f.allowed_in(channel)),
            )
        })
        .collect();
    serde_json::json!({
        "channel": channel.as_str(),
        "features": features,
    })
}

/// Report the resolved release channel and which gated features it allows —
/// the authoritative answer to "why is remote/AI/observe disabled?".
/// The mcp-proxy hub section: the keyring credential backend, plus a live probe
/// of each exposed upstream (spawn/handshake, exposed/hidden tool counts,
/// scope). Skipped entirely when no `[mcp_servers.<name>.proxy]` opts in — the
/// default-deny floor means an unconfigured proxy is silent and free.
fn mcp_proxy_report(cfg: &Config) {
    use thegn_core::mcp::config::McpServerConfig;
    use thegn_core::seam::Availability;

    let any_exposed = cfg
        .mcp_servers
        .values()
        .any(McpServerConfig::is_proxy_exposed);
    if !any_exposed {
        return;
    }

    outln!("");
    outln!("MCP proxy hub ([mcp_proxy] + [mcp_servers.<name>.proxy])");

    // Keyring credential backend Probe (custody for `keyring:` refs).
    {
        let probe = crate::secret::mcp_keyring_probe();
        let (state, detail) = match &probe.availability {
            Availability::Ready => ("ready", String::new()),
            Availability::Degraded(w) => ("degraded", format!(" — {w}")),
            Availability::Unavailable(w) => ("unavailable", format!(" — {w}")),
        };
        outln!("  keyring backend  {state}{detail}");
    }

    // Live probe: spawn each exposed upstream, handshake, count exposed/hidden.
    let hub = crate::mcp_proxy::build_hub_for_cwd(cfg);
    let now = crate::mcp_proxy::now_ms();
    outln!("  advertised tools {}", hub.tool_count());
    for r in hub.reports(now) {
        let state = if let Some(reason) = &r.withheld_reason {
            format!("withheld — {reason}")
        } else if let Some(err) = &r.error {
            format!("error — {err}")
        } else if r.running {
            format!(
                "ok (exposed {}, hidden {}, breaker {})",
                r.exposed.len(),
                r.hidden.len(),
                r.breaker
            )
        } else {
            "not running".to_string()
        };
        outln!("  {:<16} [scope={}] {state}", r.name, r.scope);
    }
}

fn channel_report() {
    let channel = crate::channel_state::current();
    outln!("Release channel");
    outln!("  channel       {}", channel.as_str());
    for f in thegn_core::channel::Feature::ALL {
        outln!(
            "  {:<13} {}",
            f.id(),
            if f.allowed_in(channel) {
                "enabled"
            } else {
                "disabled (dev-only)"
            }
        );
    }
    if matches!(channel, thegn_core::channel::Channel::Stable) {
        outln!("  note          experimental features need the dev build (`nix run .#dev`)");
        outln!("                or THEGN_CHANNEL=dev. GitHub PR/issue viewing stays available.");
    }
}

/// The filesystem the sampler measures for the disk metric — the worktrees dir,
/// or an explicit `[stats] disk_path`. Mirrors the live sampler in `run.rs` so
/// the coverage report reflects what the compositor actually samples.
fn sampler_disk_path(cfg: &Config) -> std::path::PathBuf {
    if cfg.stats.disk_path.trim().is_empty() {
        std::path::PathBuf::from(&cfg.worktrees_dir)
    } else {
        std::path::PathBuf::from(thegn_core::util::expand_tilde(&cfg.stats.disk_path))
    }
}

/// Take two samples (CPU/net/reclaim are deltas, so the first only primes) and
/// classify per-family coverage. The ~300ms warmup is why this is CLI-only and
/// never rides the compositor's fast path.
fn sample_metric_coverage(cfg: &Config) -> Vec<thegn_metrics::FamilyReport> {
    let mut s = thegn_metrics::StatsSampler::new(sampler_disk_path(cfg));
    let _ = s.sample(); // best-effort: warm-up sample: CPU/net are deltas, so the first only primes
    std::thread::sleep(std::time::Duration::from_millis(300));
    let snap = s.sample();
    thegn_metrics::coverage(&snap)
}

/// `--json` twin of [`system_metrics_report`]: one object keyed by family with
/// `available` and, when absent, a `reason`.
fn system_metrics_json(cfg: &Config) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = sample_metric_coverage(cfg)
        .into_iter()
        .map(|r| {
            let v = match r.coverage {
                thegn_metrics::Coverage::Available => serde_json::json!({ "available": true }),
                thegn_metrics::Coverage::Absent(reason) => {
                    serde_json::json!({ "available": false, "reason": reason.word() })
                }
            };
            (r.family.key().to_string(), v)
        })
        .collect();
    serde_json::Value::Object(map)
}

/// Human report: one line per metric family with `available`, or `absent
/// (<reason>)`. Matches exactly what the masthead widgets and monitor tabs show,
/// so a puzzling missing widget has an authoritative explanation here.
fn system_metrics_report(cfg: &Config) {
    outln!("System metrics coverage (this platform + machine)");
    for r in sample_metric_coverage(cfg) {
        let status = match r.coverage {
            thegn_metrics::Coverage::Available => "available".to_string(),
            thegn_metrics::Coverage::Absent(reason) => format!("absent ({})", reason.word()),
        };
        outln!("  {:<13} {}", r.family.key(), status);
    }
}

/// One harness's probe: binary on PATH, credential home present, logged-in
/// (auth marker), and a session store on disk. Pure reads only — a probe never
/// launches the harness. Mirrors the provider-seams `Probe` shape.
struct HarnessProbe {
    id: &'static str,
    binary: Option<String>,
    home: Option<std::path::PathBuf>,
    logged_in: bool,
    store_found: bool,
    caps: Vec<&'static str>,
}

fn probe_harness(h: &'static dyn thegn_core::harness::Harness) -> HarnessProbe {
    let binary = thegn_core::util::which_path(h.interactive_command());
    // The effective on-host credential home (only when it already exists) — the
    // same resolution the sandbox carve and usage discovery use.
    let home = thegn_core::account::provider(h.id())
        .and_then(thegn_core::account::effective_config_dir)
        .map(std::path::PathBuf::from);
    let logged_in = match (&home, h.home()) {
        (Some(dir), Some(spec)) => dir.join(spec.auth_marker).exists(),
        _ => false,
    };
    let store_found = match (&home, h.session_layout()) {
        (Some(dir), Some(layout)) => dir.join(layout.store_subdir).is_dir(),
        _ => false,
    };
    HarnessProbe {
        id: h.id(),
        binary,
        home,
        logged_in,
        store_found,
        caps: h.caps().names(),
    }
}

/// Report every registered coding-agent harness: its capabilities and the
/// probe (binary, credential home, login state, session store). The seam's
/// `thegn doctor` surface — "why can't thegn resume/see-usage-for X?" in a
/// glance.
fn harness_report(cfg: &Config) {
    outln!("Coding-agent harnesses ([[agents]] / [usage] providers)");
    for h in thegn_core::harness::HARNESSES {
        let p = probe_harness(*h);
        let caps = if p.caps.is_empty() {
            "(none)".to_string()
        } else {
            p.caps.join(", ")
        };
        outln!("  {:<12} caps: {caps}", p.id);
        outln!(
            "               binary: {}",
            p.binary.as_deref().unwrap_or("MISSING (not on PATH)")
        );
        match &p.home {
            Some(dir) => {
                outln!("               home: {}", dir.display());
                outln!(
                    "               login: {} · session store: {}",
                    if p.logged_in { "yes" } else { "no" },
                    if h.session_layout().is_none() {
                        "n/a"
                    } else if p.store_found {
                        "found"
                    } else {
                        "none"
                    },
                );
            }
            None => outln!("               home: (none — no relocatable credential home)"),
        }
    }
    // What each configured entry actually launches as: harness, model, env
    // overlay keys, permissions — the effective view after resolution, which
    // is what a "why did my worker run the wrong tier" question needs.
    if !cfg.agents.is_empty() {
        outln!("  [[agents]] (effective):");
        for a in &cfg.agents {
            match thegn_core::agent_task::effective_agent(cfg, &a.name, None) {
                Ok(e) => outln!(
                    "    {:<20} harness: {:<8} model: {:<24} env: {} · permissions: {}",
                    a.name,
                    e.harness,
                    e.model.as_deref().unwrap_or("(default)"),
                    if e.env.is_empty() {
                        "(none)".to_string()
                    } else {
                        e.env.keys().cloned().collect::<Vec<_>>().join(",")
                    },
                    e.permissions.len(),
                ),
                Err(why) => outln!("    {:<20} INVALID: {why}", a.name),
            }
        }
    }
}

/// The effective launch view of every `[[agents]]` entry (harness, model, env
/// overlay keys, permissions) — `doctor --json`'s `agents`.
fn agents_json(cfg: &Config) -> serde_json::Value {
    let agents: Vec<serde_json::Value> = cfg
        .agents
        .iter()
        .map(
            |a| match thegn_core::agent_task::effective_agent(cfg, &a.name, None) {
                Ok(e) => serde_json::json!({
                    "name": a.name,
                    "harness": e.harness,
                    "model": e.model,
                    "env_keys": e.env.keys().cloned().collect::<Vec<_>>(),
                    "permissions": e.permissions,
                }),
                Err(why) => serde_json::json!({ "name": a.name, "error": why }),
            },
        )
        .collect();
    serde_json::Value::Array(agents)
}

fn harness_json() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = thegn_core::harness::HARNESSES
        .iter()
        .map(|h| {
            let p = probe_harness(*h);
            serde_json::json!({
                "id": p.id,
                "caps": p.caps,
                "binary": p.binary,
                "home": p.home.as_ref().map(|d| d.display().to_string()),
                "logged_in": p.logged_in,
                "session_store": if h.session_layout().is_none() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::from(p.store_found)
                },
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

/// The config-resolved `thegn mcp serve` scope ceiling (without the
/// per-invocation `--scopes` flag): global `[mcp.serve]` narrowed by the active
/// profile overlay, defaulting to `read`. Reported so an operator can see what
/// an MCP server would grant before `--scopes` narrows it further.
fn resolved_serve_ceiling(
    cfg: &Config,
) -> (
    thegn_core::control::ScopeSet,
    thegn_core::control::ScopeClamp,
) {
    use thegn_core::control::{Scope, ScopeSet, resolve_serve_scopes};
    let global = cfg.mcp.serve.scope_set();
    let profile = cfg
        .profiles
        .get(&thegn_core::profile::name())
        .and_then(|p| p.mcp_serve.scope_set());
    resolve_serve_scopes(global, profile, None, None, ScopeSet::of(&[Scope::Read]))
}

fn mcp_serve_scopes_report(cfg: &Config) {
    let (eff, clamp) = resolved_serve_ceiling(cfg);
    outln!("MCP serve scopes ([mcp.serve])");
    let csv = eff.to_csv();
    outln!(
        "  ceiling       [{}] (clamped by: {})",
        if csv.is_empty() { "none" } else { &csv },
        clamp.as_str()
    );
    outln!("  note          `--scopes` at invocation intersects this (clamp-only, never widens)");
}

fn mcp_serve_scopes_json(cfg: &Config) -> serde_json::Value {
    let (eff, clamp) = resolved_serve_ceiling(cfg);
    serde_json::json!({
        "ceiling": eff.to_csv(),
        "clamped_by": clamp.as_str(),
    })
}

/// One log sink's identity for the identification block: display name, path,
/// current size (bytes; `None` if absent), and rotation cap in MiB.
pub(crate) struct SinkInfo {
    pub name: &'static str,
    pub path: std::path::PathBuf,
    pub size: Option<u64>,
    pub cap_mb: u64,
}

/// The four log sinks doctor + the bundle know about, with their live sizes.
pub(crate) fn log_sinks(cfg: &Config) -> Vec<SinkInfo> {
    let dir = cfg.log.dir_path();
    let size = |p: &std::path::Path| std::fs::metadata(p).ok().map(|m| m.len());
    [
        ("thegn.log", dir.join("thegn.log"), cfg.log.rotation_size_mb),
        (
            "thegn-daemon.log",
            dir.join("thegn-daemon.log"),
            cfg.log.rotation_size_mb,
        ),
        (
            "thegn-stderr.log",
            dir.join("thegn-stderr.log"),
            cfg.log.stderr_cap_mb,
        ),
        (
            "audit.log",
            thegn_core::util::thegn_dir().join("audit.log"),
            cfg.log.stderr_cap_mb,
        ),
    ]
    .into_iter()
    .map(|(name, path, cap_mb)| SinkInfo {
        name,
        size: size(&path),
        path,
        cap_mb,
    })
    .collect()
}

/// Daemon liveness for the identification block: reachable (a live heartbeat),
/// stale (a registry row past its TTL — a crashed/wedged daemon), or absent.
/// Version is the daemon's reported `CARGO_PKG_VERSION` when a row exists.
fn daemon_health() -> (&'static str, Option<String>) {
    use thegn_core::store::ControlStore;
    let Some(db) = thegn_core::db::Db::open().ok() else {
        return ("unknown (no DB)", None);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let ttl = thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS;
    let scope = thegn_core::util::xdg_state_home()
        .join("thegn")
        .to_string_lossy()
        .into_owned();
    // A live row (fresh heartbeat) in any scope means reachable; else a stale row
    // means crashed/wedged; else none.
    let all = db.daemons().unwrap_or_default();
    let live = db.live_daemons(&scope, now, ttl).unwrap_or_default();
    if let Some(row) = live.first() {
        ("reachable", Some(row.version.clone()))
    } else if let Some(row) = all.iter().max_by_key(|r| r.heartbeat_at) {
        (
            "stale (heartbeat past TTL — crashed or wedged)",
            Some(row.version.clone()),
        )
    } else {
        ("no daemon registered", None)
    }
}

/// Report thegn's own identity: version, channel, build, OS, the daemon's
/// version + reachability, the `[log]` sinks with sizes/caps, and recent crash
/// reports — the first questions any bug report needs answered.
fn identification_report(cfg: &Config) {
    let id = crate::diag::identity(crate::channel_state::current().as_str());
    outln!("Installation");
    outln!("  version       {}", id.version);
    outln!("  channel       {}", id.channel);
    outln!(
        "  build         {}",
        id.build.as_deref().unwrap_or("(unknown)")
    );
    outln!("  os/arch       {}/{}", id.os, id.arch);
    let (dstate, dver) = daemon_health();
    outln!(
        "  daemon        {} (version {})",
        dstate,
        dver.as_deref().unwrap_or("unknown")
    );
    outln!("  run id        {}", thegn_core::diagnostics::run_id());
    outln!("");
    outln!("Logs ([log])");
    outln!("  level         {}", cfg.log.level.as_str());
    outln!("  dir           {}", cfg.log.dir_path().display());
    for s in log_sinks(cfg) {
        let size = s
            .size
            .map(|b| format!("{:.1} KiB", b as f64 / 1024.0))
            .unwrap_or_else(|| "(absent)".into());
        outln!(
            "  {:<14} {size} (cap {} MiB)  {}",
            s.name,
            s.cap_mb,
            s.path.display()
        );
    }
    let reports = thegn_core::diagnostics::list_reports();
    let unack = thegn_core::diagnostics::unacknowledged_reports().len();
    outln!("");
    outln!("Crash reports ([diagnostics])");
    outln!(
        "  dir           {}",
        thegn_core::diagnostics::crash_dir().display()
    );
    if reports.is_empty() {
        outln!("  reports       (none)");
    } else {
        outln!(
            "  reports       {} retained, {unack} unacknowledged",
            reports.len()
        );
        for p in reports.iter().rev().take(5) {
            outln!(
                "                {}",
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );
        }
    }
}

/// The identification block for `--json`.
fn identification_json(cfg: &Config) -> serde_json::Value {
    let id = crate::diag::identity(crate::channel_state::current().as_str());
    let (dstate, dver) = daemon_health();
    let sinks: Vec<serde_json::Value> = log_sinks(cfg)
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "path": s.path.display().to_string(),
                "size_bytes": s.size,
                "cap_mb": s.cap_mb,
            })
        })
        .collect();
    let reports: Vec<String> = thegn_core::diagnostics::list_reports()
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    serde_json::json!({
        "version": id.version,
        "channel": id.channel,
        "build": id.build,
        "os": id.os,
        "arch": id.arch,
        "run_id": thegn_core::diagnostics::run_id(),
        "daemon": { "state": dstate, "version": dver },
        "log": {
            "level": cfg.log.level.as_str(),
            "dir": cfg.log.dir_path().display().to_string(),
            "sinks": sinks,
        },
        "crash": {
            "dir": thegn_core::diagnostics::crash_dir().display().to_string(),
            "reports": reports,
            "unacknowledged": thegn_core::diagnostics::unacknowledged_reports().len(),
        },
    })
}

/// The full `doctor --json` report, reused by `thegn doctor bundle`. Recomputes
/// terminal detection so it is standalone.
pub(crate) fn doctor_json(cfg: &Config) -> serde_json::Value {
    let env = TermEnv::from_env();
    let detected = thegn_core::termcaps::detect(&env);
    let resolved = crate::run::resolve_termcaps(cfg);
    let probe = crate::probe::probe_outer_terminal_cli();
    let probed = crate::run::resolve_termcaps_with_probe(cfg, probe.as_ref());
    serde_json::json!({
        "identification": identification_json(cfg),
        "channel": channel_json(),
        "completions": completions_json(),
        "core_deps": core_deps_json(),
        "env": {
            "TERM": env.term,
            "COLORTERM": env.colorterm,
            "TERM_PROGRAM": env.term_program,
            "TERM_PROGRAM_VERSION": env.term_program_version,
            "LC_TERMINAL": env.lc_terminal,
            "VTE_VERSION": env.vte_version,
            "NO_COLOR": env.no_color,
            "WT_SESSION": env.wt_session,
            "LANG": env.lang,
            "LC_ALL": env.lc_all,
            "LC_CTYPE": env.lc_ctype,
        },
        "config": {
            "color": cfg.theme.color.as_str(),
            "glyphs": cfg.theme.glyphs.as_str(),
            "agent_glyphs": cfg.theme.agent_glyphs.as_str(),
            "undercurl": cfg.theme.undercurl.as_str(),
        },
        "detected": caps_json(&detected),
        "resolved": caps_json(&resolved),
        "probe": probe.as_ref().map(|p| serde_json::json!({
            "responded": p.responded,
            "terminal": p.terminal_name,
            "modern": p.modern,
        })),
        "resolved_with_probe": caps_json(&probed),
        // Kept out of the `probe` object above so the three states survive a
        // skipped probe: `true` / `false` / `null` (unknown ⇒ assume it works).
        "keyboard": {
            "modify_other_keys": probe.as_ref().and_then(|p| p.modify_other_keys),
            "kitty_keyboard": probe.as_ref().and_then(|p| p.kitty_keyboard),
            "ctrl_digits_reportable": probe.as_ref().and_then(|p| p.ctrl_digit_reportable()),
        },
        "sandbox": sandbox_json(cfg),
        "remote_sandbox": remote_sandbox_json(cfg),
        "provider_cache": provider_cache_json(cfg),
        "managed_tools": managed_tools_json(cfg),
        "mcp_servers": mcp_servers_json(cfg),
        "network": network_json(cfg),
        "providers": providers_json(cfg),
        "merge_guard": merge_guard_json(cfg),
        "mobile_access": mobile_access_json(cfg),
        "lsp": lsp_json(cfg),
        "system_metrics": system_metrics_json(cfg),
        "source_control": source_control_json(cfg),
        "harnesses": harness_json(),
        "agents": agents_json(cfg),
        "mcp_serve": mcp_serve_scopes_json(cfg),
        "model_proxy": model_proxy_json(cfg),
    })
}

/// Reports the model proxy: a single quiet line when disabled, else enabled
/// state, listen (warning when non-loopback), reachability, and per-provider
/// kind + SecretRef resolvability (never values).
fn model_proxy_report(cfg: &Config) {
    use thegn_core::seam::Kind;
    let mp = &cfg.model_proxy;
    if !mp.enabled {
        outln!("Model proxy ([model_proxy])  disabled");
        return;
    }
    outln!("Model proxy ([model_proxy])");
    let loopback = mp.listen_is_loopback();
    outln!(
        "  listen        {}{}",
        mp.listen,
        if loopback {
            ""
        } else {
            "  WARNING: non-loopback exposes metered spend"
        }
    );
    outln!(
        "  reachable     {}",
        yn(crate::model_proxy_daemon::probe_up(mp))
    );
    outln!("  routing       {}", mp.routing.as_str());
    outln!("  usage_aware   {}", yn(mp.usage_aware));
    outln!("  providers     {}", mp.providers.len());
    for p in &mp.providers {
        let kind_note = if p.kind.is_reserved() {
            " (reserved — not routed)"
        } else {
            ""
        };
        // Report resolvability of the first key ref, never its value.
        let key_state = key_ref_state(&p.api_key);
        outln!(
            "    - {:<14} [{}]{}  key: {}",
            p.name,
            p.kind.as_str(),
            kind_note,
            key_state
        );
    }
    outln!(
        "  routes        {}",
        mp.routes
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for r in &mp.routes {
        outln!(
            "    {} → {}",
            r.name,
            r.backends
                .iter()
                .map(|b| format!("{}:{}", b.provider, b.model))
                .collect::<Vec<_>>()
                .join(" → ")
        );
    }
    for w in mp.warnings() {
        outln!("  ! {w}");
    }
}

/// Describes a provider `api_key` SecretRef's resolvability without ever
/// exposing its value.
fn key_ref_state(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return "none (keyless/subscription)".to_string();
    }
    if let Some(var) = raw.strip_prefix("env:") {
        let var = var.trim();
        return if std::env::var_os(var).is_some() {
            format!("env:{var} (set)")
        } else {
            format!("env:{var} (NOT SET)")
        };
    }
    if let Some(path) = raw.strip_prefix("file:") {
        let path = path.trim();
        return if std::path::Path::new(path).exists() {
            format!("file:{path} (present)")
        } else {
            format!("file:{path} (MISSING)")
        };
    }
    "INVALID — must be env:VAR or file:PATH".to_string()
}

fn model_proxy_json(cfg: &Config) -> serde_json::Value {
    use thegn_core::seam::Kind;
    let mp = &cfg.model_proxy;
    if !mp.enabled {
        return serde_json::json!({"enabled": false});
    }
    serde_json::json!({
        "enabled": true,
        "listen": mp.listen,
        "loopback": mp.listen_is_loopback(),
        "reachable": crate::model_proxy_daemon::probe_up(mp),
        "routing": mp.routing.as_str(),
        "usage_aware": mp.usage_aware,
        "providers": mp.providers.iter().map(|p| serde_json::json!({
            "name": p.name,
            "kind": p.kind.as_str(),
            "reserved": p.kind.is_reserved(),
            "key": key_ref_state(&p.api_key),
        })).collect::<Vec<_>>(),
        "routes": mp.routes.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
        "warnings": mp.warnings(),
    })
}

pub fn run(cfg: &Config, json: bool) -> Result<()> {
    if json {
        outln!("{}", serde_json::to_string_pretty(&doctor_json(cfg))?);
        return Ok(());
    }

    let env = TermEnv::from_env();
    let resolved = crate::run::resolve_termcaps(cfg);
    // Ask the terminal itself, exactly as the compositor does at startup. `None`
    // when stdout isn't a tty (so `doctor --json | jq` and CI are unaffected) or
    // `THEGN_PROBE_MS=0`. Reporting only the env answer is how `doctor` came to
    // contradict the compositor over ssh/tmux — the one case the probe exists for.
    let probe = crate::probe::probe_outer_terminal_cli();
    let probed = crate::run::resolve_termcaps_with_probe(cfg, probe.as_ref());

    let show = |k: &str, v: &Option<String>| {
        outln!("  {k:<13} {}", v.as_deref().unwrap_or("(unset)"));
    };
    identification_report(cfg);
    outln!("");
    channel_report();
    outln!("");
    core_deps_report();
    outln!("");
    outln!("Terminal environment");
    show("TERM", &env.term);
    show("COLORTERM", &env.colorterm);
    show("TERM_PROGRAM", &env.term_program);
    show("TERM_PROG_VER", &env.term_program_version);
    // The one that survives ssh, so it explains an otherwise-baffling
    // "why is my iTerm2 detected as a plain 256-color terminal" one hop away.
    show("LC_TERMINAL", &env.lc_terminal);
    show("VTE_VERSION", &env.vte_version);
    outln!("  {:<13} {}", "NO_COLOR", yn(env.no_color));
    show("WT_SESSION", &env.wt_session);
    show("LANG", &env.lang);
    show("LC_ALL", &env.lc_all);
    show("LC_CTYPE", &env.lc_ctype);

    outln!("");
    outln!("Config modes ([theme])");
    outln!("  color         {}", cfg.theme.color.as_str());
    outln!("  glyphs        {}", cfg.theme.glyphs.as_str());
    outln!("  agent glyphs  {}", cfg.theme.agent_glyphs.as_str());
    outln!("  undercurl     {}", cfg.theme.undercurl.as_str());

    outln!("");
    outln!("Resolved capabilities (env + config)");
    outln!("  color         {}", color_str(resolved.color));
    outln!("  glyphs        {}", unicode_str(resolved.unicode));
    outln!("  undercurl     {}", yn(resolved.undercurl));
    outln!("  mouse         {}", yn(resolved.mouse));
    outln!("  osc52 copy    {}", yn(resolved.osc52));
    outln!("  sync output   {}", yn(resolved.sync_output));
    // Keyboard reporting comes from the probe, not env+config — but it belongs
    // with the other "what will actually work" answers, which is where people
    // look when a chord does nothing. `just term-check` greps only the `color`
    // and `glyphs` rows, so appending here is safe.
    let ctrl_digits = probe.as_ref().and_then(|p| p.ctrl_digit_reportable());
    outln!("  keyboard      {}", keyboard_str(ctrl_digits));
    if ctrl_digits == Some(false) {
        for line in keyboard_remedy(std::env::var_os("TMUX").is_some()) {
            outln!("                {line}");
        }
    }

    outln!("");
    providers_report(cfg);

    mcp_proxy_report(cfg);

    outln!("");
    secrets_report(cfg);

    outln!("");
    hostkey_report();

    outln!("");
    exposure_report(cfg);

    outln!("");
    control_surface_report();

    outln!("");
    completions_report();

    outln!("");
    harness_report(cfg);

    outln!("");

    outln!("Outer-terminal probe (DA + XTVERSION) — what the compositor installs");
    match &probe {
        None => outln!("  probe         skipped (not a tty, or THEGN_PROBE_MS=0)"),
        Some(p) => {
            outln!("  answered      {}", yn(p.responded));
            outln!(
                "  terminal      {}",
                p.terminal_name.as_deref().unwrap_or("(unnamed)")
            );
            outln!("  known-modern  {}", yn(p.modern));
        }
    }
    if probed == resolved {
        outln!("  effect        none — same as above");
    } else {
        // The disagreement made visible. Only `auto` knobs can be upgraded, so
        // an explicit `[theme]` value still wins and this stays quiet.
        outln!("  color         {}", color_str(probed.color));
        outln!("  glyphs        {}", unicode_str(probed.unicode));
        outln!("  undercurl     {}", yn(probed.undercurl));
        outln!("  sync output   {}", yn(probed.sync_output));
    }

    outln!("");
    macos_report(&env);

    outln!("");
    pane_daemon_report(cfg);

    outln!("");
    mobile_access_report(cfg);

    outln!("");
    model_proxy_report(cfg);

    outln!("");
    sandbox_report(cfg);

    outln!("");
    hosts_report(cfg);

    outln!("");
    remote_sandbox_report(cfg);

    outln!("");
    provider_cache_report(cfg);

    outln!("");
    home_layer_report(cfg);

    outln!("");
    managed_tools_report(cfg);

    outln!("");
    mcp_servers_report(cfg);

    outln!("");
    mcp_serve_scopes_report(cfg);

    outln!("");
    network_report(cfg);

    outln!("");
    merge_guard_report(cfg);

    outln!("");
    lsp_report(cfg);

    outln!("");
    system_metrics_report(cfg);

    outln!("");
    source_control_report(cfg);

    outln!("");
    paths_report(cfg);

    outln!("");
    outln!("Summary");
    outln!("  {}", summary(&resolved));
    Ok(())
}

/// Hosts-as-resources: every [host.*] (config + DB-added), its reach, recorded
/// provisioning state, probe age, and the local-side delivery abilities the
/// registry-less transfer depends on. Detection only.
fn hosts_report(cfg: &Config) {
    outln!("Hosts ([host.*] + `thegn host add`)");
    if cfg.host.is_empty() {
        outln!("  (none — add one with `thegn host add user@box` or [host.<name>])");
        return;
    }
    let db = match thegn_core::db::Db::open() {
        Ok(db) => Some(db),
        Err(e) => {
            outln!("  (db unavailable: {e} — host sections below may be incomplete)");
            None
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for (name, hc) in &cfg.host {
        let state = cfg
            .host_binding(name)
            .and_then(|b| db.as_ref().and_then(|db| db.host_get(&b.id).ok().flatten()))
            .map(|row| {
                let age = row
                    .last_probe
                    .map(|t| format!("probed {}s ago", now.saturating_sub(t)))
                    .unwrap_or_else(|| "never probed".into());
                format!(
                    "{} · {age}",
                    row.state.durable_tag().unwrap_or("provisioning")
                )
            })
            .unwrap_or_else(|| "unprovisioned".into());
        outln!("  {name:<16} {:<6} {state}", hc.reach.as_str());
    }
    // Local delivery abilities: what the default registry-less transfer can use.
    let has = |bin: &str| which_ok(bin);
    outln!(
        "  local tools:  podman {} · skopeo {} · rsync {} (registry-less transfer wants podman or skopeo)",
        yn(has("podman")),
        yn(has("skopeo")),
        yn(has("rsync")),
    );
}

fn probe_word(p: RuntimeProbe) -> &'static str {
    match p {
        RuntimeProbe::Present => "present",
        RuntimeProbe::Absent => "absent",
        RuntimeProbe::Unreachable => "unreachable (ssh transport failed)",
    }
}

/// Every ssh `[host.*]` as `(name, Placement::Ssh)`. Iroh/cloud/local hosts have
/// no directly-probeable ssh control transport here, so they're skipped.
fn remote_ssh_placements(cfg: &Config) -> Vec<(String, Placement)> {
    let mut names: Vec<&String> = cfg.host.keys().collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let binding = cfg.host_binding(name)?;
            match binding.reach {
                thegn_core::host::Reach::Ssh(p) => Some((name.clone(), Placement::Ssh(p))),
                _ => None,
            }
        })
        .collect()
}

/// Live per-backend runtime probe for each ssh `[host.*]`: is podman/docker/bwrap
/// actually present on the remote? Distinguishes a genuinely-absent runtime from
/// an *unreachable* host (ssh failed) — the false-negative that used to strand a
/// reachable remote on `Backend::None` and ship a `cd <local-path>` to it. Each
/// probe is `ConnectTimeout=10`-bounded. No-op when no ssh hosts are configured.
fn remote_sandbox_report(cfg: &Config) {
    // The effective `[remote]` transport tuning every probe below rides.
    let t = cfg.remote.ssh_tune();
    let p = cfg.remote.control_plane_policy();
    outln!("Remote transport tuning ([remote])");
    outln!(
        "  keepalive          ServerAliveInterval={} CountMax={} TCPKeepAlive={}",
        t.keepalive_interval_secs,
        t.keepalive_count_max,
        if t.tcp_keepalive { "yes" } else { "no" }
    );
    outln!(
        "  connect/persist    ConnectTimeout={}s ControlPersist={}s",
        t.connect_timeout_secs,
        t.control_persist_secs
    );
    outln!(
        "  retry              {} attempts, {}ms..{}ms backoff; heal {:?}s",
        p.max_attempts,
        p.base_delay_ms,
        p.max_delay_ms,
        cfg.remote.heal_schedule().steps_secs
    );
    outln!("Remote sandbox runtime (live ssh probe)");
    let hosts = remote_ssh_placements(cfg);
    if hosts.is_empty() {
        outln!("  (no ssh [host.*] configured)");
        return;
    }
    let chain = shell_chain(cfg);
    for (name, placement) in hosts {
        outln!("  {name}{}", master_status(&placement));
        for bname in &chain {
            let Some(b) = Backend::parse(bname) else {
                continue;
            };
            if b == Backend::None {
                continue; // "host"/"none" have no runtime to probe
            }
            outln!(
                "    {:<10} {}",
                bname,
                probe_word(placement.probe_runtime(b.binary()))
            );
        }
    }
}

/// One-word ControlMaster status for an ssh host: is a multiplex socket on
/// disk, and does the master behind it answer `ssh -O check`?
// off-loop: doctor is a synchronous CLI; `-O check` pings a local unix socket.
#[expect(clippy::disallowed_methods)]
fn master_status(placement: &thegn_core::placement::Placement) -> String {
    let thegn_core::placement::Placement::Ssh(p) = placement else {
        return String::new();
    };
    let sock = thegn_core::remote::control_path(&p.host, p.port);
    if !sock.exists() {
        return "  (no ControlMaster socket)".into();
    }
    let alive = std::process::Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}", sock.display()))
        .args(["-O", "check", &p.host])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if alive {
        "  (ControlMaster alive)".into()
    } else {
        "  (ControlMaster socket STALE — cleared on next connect)".into()
    }
}

fn remote_sandbox_json(cfg: &Config) -> serde_json::Value {
    let chain = shell_chain(cfg);
    let mut map = serde_json::Map::new();
    for (name, placement) in remote_ssh_placements(cfg) {
        let mut backends = serde_json::Map::new();
        for bname in &chain {
            let Some(b) = Backend::parse(bname) else {
                continue;
            };
            if b == Backend::None {
                continue;
            }
            backends.insert(
                bname.clone(),
                match placement.probe_runtime(b.binary()) {
                    RuntimeProbe::Present => "present",
                    RuntimeProbe::Absent => "absent",
                    RuntimeProbe::Unreachable => "unreachable",
                }
                .into(),
            );
        }
        map.insert(name, serde_json::Value::Object(backends));
    }
    serde_json::Value::Object(map)
}

/// Sorted names of every `[env.*]` that declares a provider.
fn provider_env_names(cfg: &Config) -> Vec<&String> {
    let mut names: Vec<&String> = cfg
        .env
        .iter()
        .filter(|(_, e)| !e.provider.provider.trim().is_empty())
        .map(|(n, _)| n)
        .collect();
    names.sort();
    names
}

/// The flake devShell attr a sandbox for this env enters: the per-env
/// `[env.*.sandbox] devshell` override, else the global `[sandbox] devshell`,
/// else `default`.
fn env_devshell(cfg: &Config, e: &thegn_core::config::EnvConfig) -> String {
    e.sandbox
        .devshell
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| Some(cfg.sandbox.devshell.trim()).filter(|s| !s.is_empty()))
        .unwrap_or("default")
        .to_string()
}

/// Provider dev-env caching health: for each `[env.*]` with a provider, the
/// devShell attr its sandbox enters, whether a persistent binary cache is
/// configured, and — when `host_cache = true` — whether the resident musl bridge
/// that carries the :8484 reverse-tunnel cache is actually resolvable. A
/// host_cache with NO bridge silently no-ops: the sprite's nix.conf gets no
/// working substituter and the devShell builds from source. Detection only.
fn provider_cache_report(cfg: &Config) {
    outln!("Provider dev-env cache ([env.*.provider])");
    let names = provider_env_names(cfg);
    if names.is_empty() {
        outln!("  (no provider envs configured)");
        return;
    }
    let bridge = crate::bridge_sup::bridge_binary_path();
    for name in names {
        let e = &cfg.env[name];
        let p = &e.provider;
        let cache = if p.binary_cache_url.trim().is_empty() {
            "cache=(none — set binary_cache_url)".to_string()
        } else {
            format!("cache={}", p.binary_cache_url.trim())
        };
        outln!(
            "  {name:<12} {} · devshell={} · {cache}",
            p.provider.trim(),
            env_devshell(cfg, e),
        );
        if p.host_cache {
            match &bridge {
                Some(pth) => outln!("               host_cache: on (bridge {})", pth.display()),
                None => {
                    outln!("               host_cache: ON but BRIDGE MISSING — sprites build the");
                    outln!("               devShell FROM SOURCE. Fix: `just bridge` (or install");
                    outln!(
                        "               `.#default`, which bundles `thegn-musl`), then restart."
                    );
                }
            }
        }
    }
}

fn provider_cache_json(cfg: &Config) -> serde_json::Value {
    let bridge_present = crate::bridge_sup::bridge_binary_path().is_some();
    let mut map = serde_json::Map::new();
    for name in provider_env_names(cfg) {
        let e = &cfg.env[name];
        let p = &e.provider;
        map.insert(
            name.clone(),
            serde_json::json!({
                "provider": p.provider.trim(),
                "devshell": env_devshell(cfg, e),
                "binary_cache_url": p.binary_cache_url.trim(),
                "host_cache": p.host_cache,
                "bridge_present": bridge_present,
                "host_cache_effective": p.host_cache && bridge_present,
            }),
        );
    }
    serde_json::Value::Object(map)
}

/// Cheap PATH probe (doctor is a diagnostic CLI; subprocess is fine here).
// off-loop: doctor is a synchronous CLI verb
#[expect(clippy::disallowed_methods)]
fn which_ok(bin: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The output of `<bin> <args…>` trimmed to one line, or `None` if the binary is
/// absent or the invocation failed — used to fetch `git --version` etc. without
/// panicking when the tool is missing (the whole point of the core-deps section).
// off-loop: doctor is a synchronous CLI verb
#[expect(clippy::disallowed_methods)]
fn cmd_first_line(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|l| l.trim().to_string())
}

/// Whether the default forge reports an authenticated account
/// (`Forge::whoami` — the one identity probe). Robust to a missing `gh`
/// (returns `false`) — the section only reports, never fails.
fn gh_authenticated() -> bool {
    // off-loop: doctor is a synchronous CLI verb
    let loc = thegn_core::remote::GitLoc::Local(
        std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
    );
    crate::forge_handle::get()
        .default_forge()
        .whoami(&loc)
        .is_ok()
}

/// Report the core CLI dependencies every startup git read leans on: `git`
/// (presence + version) and `gh` (presence + auth). These are best-effort at
/// runtime, so a missing binary otherwise produces no message — doctor is where
/// it surfaces. Detection only; never panics if git/gh are absent.
/// Where thegn resolves its paths, and whether they exist.
///
/// Two different notions of "home" are in play and they normally coincide, so
/// the difference stays invisible until something moves `$HOME` (a sandbox, a CI
/// runner, a tool that reparents it) or sets `XDG_STATE_HOME`: `~` in config
/// values expands via `$HOME`, while the state/DB/gate directories follow
/// `$XDG_STATE_HOME`. A named `--profile` deliberately reroots the latter and
/// not the former. Printing both, plus each `repo_roots` entry with an
/// exists/missing marker, turns "thegn says I have no repos" into one glance.
fn paths_report(cfg: &Config) {
    outln!("Paths");
    let mark = |p: &std::path::Path| if p.exists() { "" } else { "  (MISSING)" };
    let home = thegn_core::util::home();
    outln!("  {:<15} {}{}", "$HOME", home.display(), mark(&home));
    for (label, var) in [
        ("XDG_STATE_HOME", "XDG_STATE_HOME"),
        ("XDG_CONFIG_HOME", "XDG_CONFIG_HOME"),
        ("THEGN_DIR", "THEGN_DIR"),
        ("THEGN_PROFILE", "THEGN_PROFILE"),
    ] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            outln!("  ${label:<14} {v}");
        }
    }
    for (label, path) in [
        ("state", thegn_core::util::xdg_state_home().join("thegn")),
        ("config", thegn_core::config::Config::path()),
        ("thegn dir", thegn_core::util::thegn_dir()),
        (
            "gate",
            thegn_core::util::xdg_state_home().join("thegn/gate"),
        ),
        ("worktrees", std::path::PathBuf::from(&cfg.worktrees_dir)),
        ("workspaces", std::path::PathBuf::from(&cfg.workspaces_dir)),
    ] {
        outln!("  {label:<15} {}{}", path.display(), mark(&path));
    }
    if cfg.repo_roots.is_empty() {
        outln!("  {:<15} (none configured)", "repo_roots");
    }
    for (i, r) in cfg.repo_roots.iter().enumerate() {
        let p = std::path::PathBuf::from(r);
        let label = if i == 0 { "repo_roots" } else { "" };
        outln!("  {label:<15} {}{}", p.display(), mark(&p));
    }
}

fn core_deps_report() {
    outln!("Core dependencies");
    match thegn_core::util::which_path("git") {
        Some(path) => {
            let ver =
                cmd_first_line("git", &["--version"]).unwrap_or_else(|| "unknown version".into());
            outln!("  git           {ver}");
            outln!("                {path}");
        }
        None => outln!("  git           MISSING — git reads will silently fail; install git"),
    }
    match thegn_core::util::which_path("gh") {
        Some(path) => {
            let auth = if gh_authenticated() {
                "authenticated"
            } else {
                "not authenticated (run: gh auth login)"
            };
            outln!("  gh            present, {auth}");
            outln!("                {path}");
        }
        None => outln!("  gh            absent (optional — GitHub PR/issue features degrade)"),
    }
}

/// The core-dependency surface for `--json`: git presence/version/path and gh
/// presence/auth/path. Never panics when a binary is absent.
fn core_deps_json() -> serde_json::Value {
    let git_path = thegn_core::util::which_path("git");
    let gh_path = thegn_core::util::which_path("gh");
    // Resolve version/auth up front so the json! move of the paths is unambiguous.
    let git_version = git_path
        .as_ref()
        .and_then(|_| cmd_first_line("git", &["--version"]));
    let gh_auth = gh_path.as_ref().map(|_| gh_authenticated());
    serde_json::json!({
        "git": {
            "present": git_path.is_some(),
            "path": git_path,
            "version": git_version,
        },
        "gh": {
            "present": gh_path.is_some(),
            "path": gh_path,
            "authenticated": gh_auth,
        },
    })
}

/// "Will my shell work here?" — the personal-shell layer: the resolved strategy,
/// per-env overrides, and a scan of the host dotfiles for transplant pitfalls
/// (absent `/nix/store` paths, undeclared tools). Detection only.
fn home_layer_report(cfg: &Config) {
    use thegn_core::config::ShellStrategy;
    use thegn_core::envplan::{PitfallKind, scan_dotfile};

    let g = &cfg.sandbox.home;
    outln!("Personal shell layer ([sandbox.home])");
    outln!("  strategy      {}", g.strategy);
    outln!("  portable-only {}", yn(g.portable_dotfiles_only));

    let mut envs: Vec<(&String, ShellStrategy)> = cfg
        .env
        .iter()
        .filter_map(|(n, e)| {
            e.sandbox
                .home
                .as_ref()
                .and_then(|h| h.strategy)
                .map(|s| (n, s))
        })
        .collect();
    envs.sort_by(|a, b| a.0.cmp(b.0));
    for (n, s) in envs {
        outln!("  env override  [{n}] strategy = {s}");
    }

    let candidates: Vec<String> = if !g.dotfiles.is_empty() {
        g.dotfiles.clone()
    } else {
        [
            ".gitconfig",
            ".zshrc",
            ".bashrc",
            ".profile",
            ".tmux.conf",
            ".vimrc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    };
    let Ok(home_dir) = std::env::var("HOME") else {
        outln!("  dotfiles      (HOME unset — cannot scan)");
        return;
    };
    let skips = matches!(
        g.strategy,
        ShellStrategy::Portable | ShellStrategy::ToolParity
    ) && g.portable_dotfiles_only;
    outln!("  dotfiles (scanned in $HOME):");
    let mut scanned_any = false;
    for name in candidates {
        let Ok(contents) = std::fs::read_to_string(std::path::Path::new(&home_dir).join(&name))
        else {
            continue; // missing or a directory — nothing to scan
        };
        scanned_any = true;
        let pitfalls = scan_dotfile(&name, &contents, &g.tools);
        let absent = pitfalls
            .iter()
            .filter(|p| p.kind == PitfallKind::AbsentStorePath)
            .count();
        let missing: Vec<&str> = pitfalls
            .iter()
            .filter(|p| p.kind == PitfallKind::MissingTool)
            .map(|p| p.detail.as_str())
            .collect();
        if absent == 0 && missing.is_empty() {
            outln!("    {name:<14} portable");
        } else {
            let mut notes = Vec::new();
            if absent > 0 {
                notes.push(format!(
                    "{absent} absent /nix/store path(s){}",
                    if skips {
                        " → SKIPPED (clean shell)"
                    } else {
                        ""
                    }
                ));
            }
            if !missing.is_empty() {
                notes.push(format!("undeclared tools: {}", missing.join(", ")));
            }
            outln!("    {name:<14} {}", notes.join("; "));
        }
    }
    if !scanned_any {
        outln!("    (none present on host)");
    }
}

/// Print the pane-daemon route and the control-socket path it resolves to.
///
/// The socket length is here because it is otherwise undiagnosable: an
/// over-long path silently drops panes to in-process (see
/// `handlers::startup::daemon_active`), and nothing else in the UI says so.
/// Same contract as the CPU cap — degrade quietly, surface it in `doctor`.
/// The Option-as-Meta setting for `term_program`, or `None` when we don't know
/// the terminal well enough to name one.
///
/// Not detectable at runtime — the terminal never tells us — but it is the
/// single most common way a macOS install looks broken rather than unconfigured:
/// thegn's whole primary layer is Alt-based, macOS composes characters with
/// Option by default, so `Alt-w` types `∑` and every chord reads as a dead key.
/// Naming the setting for the terminal the user is *actually in* turns a
/// mystery into one line of config. The table mirrors
/// `docs/help/terminal-compatibility.md`.
pub(crate) fn option_as_alt_hint(term_program: Option<&str>) -> Option<&'static str> {
    let p = term_program?.to_ascii_lowercase();
    Some(match () {
        _ if p.contains("ghostty") => "macos-option-as-alt = true",
        _ if p.contains("wezterm") => "send_composed_key_when_left_alt_is_pressed = false",
        _ if p.contains("kitty") => "macos_option_as_alt yes",
        _ if p.contains("alacritty") => "[window] option_as_alt = \"Both\"",
        _ if p.contains("iterm") => "Profiles → Keys → Left/Right Option: Esc+",
        // The default terminal on every Mac, the `.app` bundle's guaranteed
        // fallback, and the one the help table used to omit entirely.
        _ if p.contains("apple_terminal") => {
            "Settings → Profiles → Keyboard → Use Option as Meta key"
        }
        _ => return None,
    })
}

/// macOS-only checks. Everything here is either un-detectable (Option-as-Meta),
/// or a silent `have()`-gated degradation — which is exactly what `doctor`
/// exists to make visible. A no-op on every other platform.
fn macos_report(env: &thegn_core::termcaps::TermEnv) {
    if !cfg!(target_os = "macos") {
        return;
    }
    outln!("macOS");

    match option_as_alt_hint(env.program_name()) {
        Some(fix) => outln!("  Option as Alt {fix}"),
        None => outln!(
            "  Option as Alt set your terminal to send Alt for Option \
             (see `thegn help terminal-compatibility`)"
        ),
    }
    outln!(
        "                thegn's chords are Alt-based; macOS composes \
         characters with Option by default"
    );

    // A GUI/`.app` launch inherits launchd's environment, not a shell's, so the
    // fd ceiling there is not the one an interactive `ulimit -n` shows — and a
    // multiplexer holding a pty per pane plus git/sqlite/socket fds is exactly
    // the workload that notices. `run.rs` already raises it; report the result.
    let (soft, hard) = crate::fd_limit::current();
    // `RLIM_INFINITY` is `i64::MAX` on darwin, not `u64::MAX` — compare against
    // the platform constant or an "unlimited" hard limit prints as a 19-digit
    // number that reads like a bug.
    let show_lim = |v: u64| {
        if v >= crate::platform::rlim_infinity() {
            "unlimited".to_string()
        } else {
            v.to_string()
        }
    };
    outln!(
        "  open files    soft {} / hard {}",
        show_lim(soft),
        show_lim(hard)
    );
    // …and "unlimited" is nominal: the kernel still caps a single process at
    // `kern.maxfilesperproc`, which is the number that actually bounds how many
    // panes can be live at once.
    if let Some(n) = crate::platform::max_files_per_proc() {
        outln!("                kernel ceiling {n} (kern.maxfilesperproc)");
    }

    // `$TMPDIR` is what `daemon::short_runtime_dir` relocates the pane-daemon
    // socket into: macOS has no `$XDG_RUNTIME_DIR`, so without a usable TMPDIR
    // the socket stays in the deep state dir and can exceed `sun_path` (104 on
    // darwin vs Linux's 108) — which silently drops the session to in-process
    // panes. Unset TMPDIR happens for real under launchd and scrubbed ssh envs.
    match std::env::var_os("TMPDIR").filter(|v| !v.is_empty()) {
        Some(t) => outln!("  TMPDIR        {}", std::path::Path::new(&t).display()),
        None => outln!(
            "  TMPDIR        (unset) — the pane-daemon socket cannot be \
             shortened; keep XDG_STATE_HOME short"
        ),
    }

    // The macOS integrations, each of which degrades silently to nothing.
    outln!("  integrations");
    for (bin, what) in [
        ("osascript", "desktop notifications"),
        ("afplay", "chime"),
        ("pbcopy", "clipboard copy"),
        ("pbpaste", "clipboard paste"),
        (
            "fc-list",
            "font picker (optional; falls back to font directories)",
        ),
        ("mediaremote-adapter", "media badge beyond Spotify/Music"),
    ] {
        let present = thegn_core::util::have(bin);
        outln!(
            "    {:<20} {:<9} {what}",
            bin,
            if present { "present" } else { "MISSING" }
        );
    }
}

/// Mobile access: the push-to-phone channel, the guarded command inbox, and
/// whether `mosh-server` is present for the phone-terminal path (mosh app →
/// this host → `thegn` attach, sessions kept warm by the daemon). Detection
/// only; the push provider itself is also in the Providers section above.
fn mobile_access_report(cfg: &Config) {
    let push = &cfg.notifications.push;
    let inbox = &push.inbox;
    outln!("Mobile access ([notifications.push])");
    // Outbound push channel.
    if push.is_configured() {
        outln!(
            "  push out      {} → {}/{}  (floor: {})",
            push.kind.as_str(),
            push.server.trim_end_matches('/'),
            push.topic,
            push.min_priority().as_str(),
        );
    } else if push.kind.is_reserved() {
        outln!(
            "  push out      {} (reserved — not implemented in this build)",
            push.kind.as_str()
        );
    } else {
        outln!("  push out      off (set [notifications.push] topic to enable)");
    }
    // Inbound command inbox.
    if !inbox.enabled {
        outln!("  command inbox off ([notifications.push.inbox] enabled = false)");
    } else if let Some(reason) = inbox.startup_block_reason() {
        outln!("  command inbox CONFIG ERROR — will not start: {reason}");
    } else if !cfg.daemon.enabled {
        outln!("  command inbox enabled but [daemon] enabled = false — the inbox needs a daemon");
    } else {
        outln!(
            "  command inbox on: topic {:?}, {} allowed cap(s), ceiling {}{}",
            inbox.topic,
            inbox.allow_set().len(),
            inbox.scopes.join(","),
            if inbox.reply_topic.is_empty() {
                String::new()
            } else {
                format!(", replies → {:?}", inbox.reply_topic)
            },
        );
    }
    // Phone-terminal path: mosh app → host → `thegn` attach.
    let mosh = thegn_core::util::which_path("mosh-server").is_some();
    outln!(
        "  mosh-server   {} — phone terminal (Blink/Termius → mosh → `thegn`) {}",
        if mosh { "present" } else { "absent" },
        if mosh {
            "works; the daemon keeps sessions warm across drops"
        } else {
            "needs mosh-server on this host"
        }
    );
    outln!("                see `thegn help mobile-access`");
}

fn mobile_access_json(cfg: &Config) -> serde_json::Value {
    let push = &cfg.notifications.push;
    let inbox = &push.inbox;
    serde_json::json!({
        "push": {
            "configured": push.is_configured(),
            "kind": push.kind.as_str(),
            "reserved": push.kind.is_reserved(),
            "server": push.server,
            "topic": push.topic,
            "min_priority": push.min_priority().as_str(),
        },
        "inbox": {
            "enabled": inbox.enabled,
            "config_error": inbox.startup_block_reason(),
            "needs_daemon": inbox.enabled && !cfg.daemon.enabled,
            "allowed": inbox.allow_set().len(),
            "scopes": inbox.scopes,
            "reply_topic": inbox.reply_topic,
        },
        "mosh_server": thegn_core::util::which_path("mosh-server").is_some(),
    })
}

fn pane_daemon_report(cfg: &Config) {
    use thegn_core::config_daemon::{check_socket_path_len, max_socket_path_len};

    outln!("Pane daemon ([daemon])");
    let sock = crate::daemon::socket_path(&cfg.daemon);
    let max = max_socket_path_len(cfg!(target_os = "linux"));
    let fit = check_socket_path_len(&sock, max, cfg!(windows));
    let len = sock.as_os_str().as_encoded_bytes().len();

    outln!("  enabled       {}", yn(cfg.daemon.enabled));
    outln!("  socket        {}", sock.display());
    if cfg!(windows) {
        // The endpoint is a fixed-length hash of the path, so length is moot.
        outln!("  endpoint      named pipe (path length not a constraint)");
    } else {
        outln!("  path length   {len} bytes (limit {max})");
    }
    let route = match (&fit, cfg.daemon.enabled) {
        (Err(t), _) => format!(
            "DEGRADED — in-process panes; socket is {} bytes over the limit. \
             Fix: set [daemon] socket to a shorter path",
            t.len - t.max
        ),
        (Ok(()), false) => "in-process panes ([daemon] enabled = false)".to_string(),
        (Ok(()), true) => "daemon-backed panes (survive UI detach)".to_string(),
    };
    outln!("  route         {route}");
    // The env kill-switches are the other reason panes go in-process; naming
    // them here stops a stray export looking like a daemon bug.
    for var in ["THEGN_NO_DAEMON", "THEGN_BENCH_FIRST_FRAME_EXIT"] {
        if std::env::var_os(var).is_some() {
            outln!("  override      {var} set — forcing in-process panes");
        }
    }
}

fn hook_kind_str(k: Option<thegn_core::merge_guard::HookKind>) -> &'static str {
    use thegn_core::merge_guard::HookKind as K;
    match k {
        None => "(absent)",
        Some(K::Current) => "thegn guard",
        Some(K::StaleOurs) => "thegn guard (older revision)",
        Some(K::Shim) => "pre-commit framework shim",
        Some(K::Foreign) => "another hook (preserved)",
    }
}

/// The `pre-merge-commit` arrangement. Reported because it is otherwise
/// invisible — the installer logs its plan at `debug` only — and one bad shape
/// fails *every* `git merge` in the checkout with a message about hook plumbing
/// that names neither thegn nor the real cause.
fn merge_guard_report(cfg: &Config) {
    use thegn_core::merge_guard;

    outln!("Merge guard ([git] merge_guard)");
    outln!("  enabled       {}", yn(cfg.git.merge_guard));
    let hooks = thegn_core::util::git_common_dir(&std::env::current_dir().unwrap_or_default())
        .join("hooks");
    outln!("  hooks dir     {}", hooks.display());
    match merge_guard::audit(&hooks) {
        Err(e) => outln!("  status        unreadable ({e})"),
        Ok(a) => {
            outln!("  slot          {}", hook_kind_str(a.slot));
            outln!("  .legacy       {}", hook_kind_str(a.legacy));
            outln!("  .thegn-orig   {}", hook_kind_str(a.chained));
            outln!("  guard runs    {}", yn(a.guard_runs()));
            match a.fault() {
                None => outln!("  status        OK"),
                Some(f) => {
                    let what = match f {
                        merge_guard::Fault::ShimChained => {
                            "BROKEN — a framework shim is parked at .thegn-orig; \
                             invoked from there it fails migration mode"
                        }
                        merge_guard::Fault::NotInstalled => {
                            "not installed — a sandboxed merge here is unguarded"
                        }
                    };
                    outln!("  status        {what}");
                    outln!("  fix           {}", f.remedy());
                }
            }
        }
    }
}

fn merge_guard_json(cfg: &Config) -> serde_json::Value {
    let hooks = thegn_core::util::git_common_dir(&std::env::current_dir().unwrap_or_default())
        .join("hooks");
    let audit = thegn_core::merge_guard::audit(&hooks).ok();
    serde_json::json!({
        "enabled": cfg.git.merge_guard,
        "hooks_dir": hooks.display().to_string(),
        "slot": audit.map(|a| hook_kind_str(a.slot)),
        "legacy": audit.map(|a| hook_kind_str(a.legacy)),
        "chained": audit.map(|a| hook_kind_str(a.chained)),
        "guard_runs": audit.map(|a| a.guard_runs()),
        "fault": audit.and_then(|a| a.fault()).map(|f| match f {
            thegn_core::merge_guard::Fault::ShimChained => "shim_chained",
            thegn_core::merge_guard::Fault::NotInstalled => "not_installed",
        }),
    })
}

/// The `merge-tree --write-tree` floor the object-DB fold needs (git ≥ 2.38).
const MERGE_TREE_FLOOR: (u32, u32) = (2, 38);

/// Installed git version, if parseable (`git --version` → `(maj, min, patch)`).
fn git_version() -> Option<(u32, u32, u32)> {
    thegn_core::gitrefs::parse_git_version(&cmd_first_line("git", &["--version"])?)
}

/// The current repo root (the `.git` common dir's parent), for colocation and
/// merge-driver probes. `None` when `doctor` isn't run inside a repo.
fn current_repo_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    thegn_core::util::git_common_dir(&cwd)
        .parent()
        .map(|p| p.to_path_buf())
}

/// Names of custom `merge.<name>.driver` definitions in the repo's git config
/// (the drivers a `.gitattributes merge=<name>` can route a fold into).
// off-loop: doctor is a synchronous CLI verb.
#[expect(clippy::disallowed_methods)]
fn custom_merge_drivers() -> Vec<String> {
    let out = thegn_core::util::git_cmd(std::path::Path::new("."))
        .args(["config", "--get-regexp", r"^merge\..*\.driver$"])
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        // `merge.<name>.driver` → `<name>`.
        .filter_map(|k| {
            k.strip_prefix("merge.")
                .and_then(|r| r.strip_suffix(".driver"))
        })
        .map(|s| s.to_string())
        .collect()
}

/// Cheap, non-interactive signing probe: sign a throwaway commit object over the
/// empty tree with `-S` and `GIT_TERMINAL_PROMPT=0`. Success ⇒ a fold can sign
/// headlessly; the created object is dangling (unreferenced) and GC-collected.
/// Only called when `sign_commits` is on (opt-in posture, not a seam probe).
// off-loop: doctor is a synchronous CLI verb.
#[expect(clippy::disallowed_methods)]
fn signing_ready() -> std::result::Result<(), String> {
    use std::path::Path;
    use std::process::Stdio;
    use thegn_core::util::git_cmd;
    // Empty tree oid via `mktree` (sha1 or sha256, no hardcoded oid).
    let tree = git_cmd(Path::new("."))
        .arg("mktree")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !tree.status.success() {
        return Err("not inside a git repository".into());
    }
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    let out = git_cmd(Path::new("."))
        .args(["commit-tree", &tree, "-S", "-m", "thegn signing probe"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// One place reporting the repo's source-control workflow posture: git version
/// against the fold's `merge-tree --write-tree` floor, jj colocation, declared
/// custom merge drivers, and (only when `sign_commits` is on) non-interactive
/// signing readiness. Cheap and local — no network.
fn source_control_report(cfg: &Config) {
    let mq = current_repo_root()
        .map(|r| cfg.repo_merge_queue(&r))
        .unwrap_or_else(|| cfg.merge_queue.clone());
    outln!("Source-control workflow posture");
    match git_version() {
        Some((maj, min, patch)) => {
            let ok = (maj, min) >= MERGE_TREE_FLOOR;
            outln!(
                "  git version   {maj}.{min}.{patch}{}",
                if ok {
                    String::new()
                } else {
                    format!(
                        "  — below {}.{}; the merge queue's object-DB fold cannot run",
                        MERGE_TREE_FLOOR.0, MERGE_TREE_FLOOR.1
                    )
                }
            );
        }
        None => outln!("  git version   unknown (git not found or unparseable)"),
    }
    outln!("  land strategy {}", mq.land_strategy.as_str());
    match current_repo_root() {
        Some(root) => {
            outln!(
                "  jj colocated  {}",
                yn(thegn_core::jj::is_colocated(&root))
            );
        }
        None => outln!("  jj colocated  (not inside a repo)"),
    }
    let drivers = custom_merge_drivers();
    if drivers.is_empty() {
        outln!("  merge drivers none declared");
    } else {
        outln!("  merge drivers {}", drivers.join(", "));
    }
    outln!("  sign commits  {}", yn(mq.sign_commits));
    if mq.sign_commits {
        match signing_ready() {
            Ok(()) => outln!("  signing       ready (a fold can sign non-interactively)"),
            Err(e) => outln!("  signing       NOT ready — {e}"),
        }
    }
    outln!("  rerere        {}", yn(mq.rerere));
}

fn source_control_json(cfg: &Config) -> serde_json::Value {
    let root = current_repo_root();
    let mq = root
        .as_ref()
        .map(|r| cfg.repo_merge_queue(r))
        .unwrap_or_else(|| cfg.merge_queue.clone());
    let ver = git_version();
    let signing = if mq.sign_commits {
        Some(match signing_ready() {
            Ok(()) => serde_json::json!({ "ready": true }),
            Err(e) => serde_json::json!({ "ready": false, "reason": e }),
        })
    } else {
        None
    };
    serde_json::json!({
        "git_version": ver.map(|(a, b, c)| format!("{a}.{b}.{c}")),
        "merge_tree_floor": format!("{}.{}", MERGE_TREE_FLOOR.0, MERGE_TREE_FLOOR.1),
        "merge_tree_ok": ver.map(|(a, b, _)| (a, b) >= MERGE_TREE_FLOOR),
        "land_strategy": mq.land_strategy.as_str(),
        "jj_colocated": root.as_ref().map(|r| thegn_core::jj::is_colocated(r)),
        "custom_merge_drivers": custom_merge_drivers(),
        "sign_commits": mq.sign_commits,
        "signing": signing,
        "rerere": mq.rerere,
    })
}

/// One-word status for a backend row. The three unusable states are kept
/// distinct because their remedies are: install something / start something you
/// already have / stop expecting it on this OS.
fn state_label(s: thegn_core::sandbox_support::BackendState) -> &'static str {
    use thegn_core::sandbox_support::BackendState as S;
    match s {
        S::Ready => "ready",
        S::NotRunning => "not running",
        S::NotInstalled => "not installed",
        S::Unsupported => "unsupported",
        S::Unreachable => "unreachable",
    }
}

/// Print the resolved sandbox boundary honestly: which backend(s) would run, the
/// isolation class each one actually provides ("what would have to fail for an
/// escape"), and the policy each hardening preset imposes.
fn sandbox_report(cfg: &Config) {
    outln!("Sandbox boundary");
    if !cfg.sandbox.enabled {
        outln!("  enabled       no  (panes run as plain host processes \u{2014} no containment)");
        return;
    }
    outln!("  enabled       yes");
    let resolved = Backend::from_config(cfg.sandbox.backend).is_some();
    if resolved {
        outln!("  backend       {}", cfg.sandbox.backend.as_str());
    } else {
        outln!(
            "  backend       {} (first usable entry below wins)",
            cfg.sandbox.backend.as_str()
        );
    }
    if let Some(rt) = cfg_oci_runtime(cfg) {
        outln!("  oci_runtime   {rt}{}", oci_runtime_status(rt));
    }
    let chain = shell_chain(cfg);
    // Actually PROBE, rather than listing the chain and calling it unknown. A
    // backend that is installed but whose service is down used to look exactly
    // like a working one here, which is how a first run could fail every pane
    // with nothing in `doctor` to explain it.
    let report = thegn_core::sandbox_support::support_report(
        &chain,
        &thegn_core::placement::Placement::Local,
        cfg_oci_runtime(cfg),
    );
    let mut all_weak = true;
    for row in &report {
        let Some(class) = isolation_of(&row.name, cfg_oci_runtime(cfg)) else {
            outln!("    {:<16} (unknown backend)", row.name);
            continue;
        };
        // Only a backend that could actually be selected counts toward "is any
        // real boundary available?" — a strong-but-unusable one is not a boundary.
        if row.state.usable()
            && !matches!(
                class,
                IsolationClass::SharedKernel | IsolationClass::HostProcess
            )
        {
            all_weak = false;
        }
        outln!(
            "    {:<16} {:<11} {} \u{2014} {}",
            row.name,
            state_label(row.state),
            class,
            class.escape_note()
        );
        if let Some(remedy) = &row.remedy {
            outln!("    {:<16} {:<11} \u{21b3} {remedy}", "", "");
        }
        // After the remedy: an unverified backend can also be stopped, and the
        // "start it" line is the more immediately actionable of the two.
        if let Some(caveat) = &row.caveat {
            outln!("    {:<16} {:<11} \u{21b3} {caveat}", "", "");
        }
    }
    match thegn_core::sandbox_support::first_ready(&report) {
        Some(r) => outln!("  selected      {} (first usable in the chain)", r.name),
        None => outln!("  selected      (none usable \u{2014} panes run on the host)"),
    }
    // Per-backend container-management ops (the Containers tab + `sandbox
    // gc/prune` surface). Reported for every usable OCI backend in the chain so
    // it's clear which ops each engine advertises (apple is list-only; podman/
    // docker carry the full set).
    let mut mgmt_lines: Vec<(String, String)> = Vec::new();
    for row in &report {
        if !row.state.usable() {
            continue;
        }
        let Some(backend) = Backend::parse(&row.name) else {
            continue;
        };
        let ops = thegn_core::sandbox_manage::manage_ops(backend);
        let names = ops.names();
        if !names.is_empty() {
            mgmt_lines.push((row.name.clone(), names.join(", ")));
        }
    }
    if !mgmt_lines.is_empty() {
        outln!("  management    (ops thegn manages its own containers with)");
        for (name, ops) in mgmt_lines {
            outln!("    {name:<16} {ops}");
        }
    }
    outln!("  network       {}", cfg.sandbox.network.as_str());
    outln!(
        "  shell profile {} ({})",
        cfg.sandbox.profile.as_str(),
        profile_policy(cfg.sandbox.profile)
    );
    cpu_cap_report(cfg);
    if all_weak {
        outln!("  note          even the strongest preset here shares the host kernel; for a");
        outln!("                stronger boundary on agent code set [sandbox] oci_runtime to");
        outln!(
            "                \"runsc\" (gVisor userspace kernel) or \"krun\" (libkrun microVM)."
        );
    }
    outln!("");
    enforcement_matrix_report(cfg);
}

/// The derived enforcement matrix for THIS host: what each reachable backend
/// actually enforces (filesystem / network isolation, resource-ceiling strength,
/// process scoping, honest class), plus the demanded isolation floor and whether
/// it is met. Aggregation-only — every cell comes from
/// [`thegn_core::sandbox_matrix::row`], derived from the same predicates the
/// resolver uses, so it can never disagree with what actually launches. The
/// ceiling cell is refined by the probed
/// [`CpuCap`](thegn_core::sandbox_cpucap::CpuCap), so a host without cgroup cpu
/// delegation shows a soft ceiling for the host-toolchain backends, not hard.
fn enforcement_matrix_report(cfg: &Config) {
    use thegn_core::sandbox_matrix;
    let os = thegn_core::sandbox_backend::host_os();
    let probed = thegn_core::sandbox_cpucap::detect_cpu_cap();
    outln!(
        "Enforcement matrix ({}, derived from the resolver)",
        os.as_str()
    );
    outln!(
        "  {:<14} {:<10} {:<22} {:<26} {:<26} class",
        "backend",
        "fs",
        "net",
        "ceiling",
        "scoping"
    );
    for r in sandbox_matrix::column_for(os) {
        let caveat = if r.verified { "" } else { "  (unverified)" };
        outln!(
            "  {:<14} {:<10} {:<22} {:<26} {:<26} {}{}",
            r.backend.label(),
            r.fs.as_str(),
            r.net.as_str(),
            r.ceiling_label(Some(probed)),
            r.scoping.as_str(),
            r.class.as_str(),
            caveat,
        );
    }
    floor_report(cfg);
}

/// The isolation floor line: the demanded minimum, the miss policy, and whether
/// the launch this host would actually pick meets it — computed with the same
/// pure [`thegn_core::sandbox_floor::decide`] the launch path uses.
fn floor_report(cfg: &Config) {
    use thegn_core::config::IsolationFloor;
    let floor = cfg.sandbox.isolation_floor;
    if floor == IsolationFloor::Off {
        outln!("  floor         (none — set [sandbox] isolation_floor to demand a minimum)");
        return;
    }
    let chain = shell_chain(cfg);
    let report = thegn_core::sandbox_support::support_report(
        &chain,
        &Placement::Local,
        cfg_oci_runtime(cfg),
    );
    // What the launch actually enters: the first usable backend's honest class,
    // or the host process when nothing usable is in the chain.
    let actual = thegn_core::sandbox_support::first_ready(&report)
        .and_then(|r| r.isolation)
        .unwrap_or(IsolationClass::HostProcess);
    // The strongest class any usable backend could give, for a concrete remedy.
    let best = report
        .iter()
        .filter(|r| r.state.usable())
        .filter_map(|r| r.isolation)
        .max_by_key(|c| c.rank().unwrap_or(0))
        .unwrap_or(actual);
    outln!(
        "  floor         {} (on miss: {})",
        floor.as_str(),
        cfg.sandbox.on_floor_miss.as_str()
    );
    use thegn_core::sandbox_floor::{FloorDecision, decide};
    match decide(floor, cfg.sandbox.on_floor_miss, actual, best) {
        FloorDecision::Ok => outln!(
            "                met — this host would launch at `{}`",
            actual
        ),
        FloorDecision::BypassProvider => {
            outln!("                provider-managed placement — floor out of scope")
        }
        FloorDecision::Degrade(m) => {
            outln!(
                "                MISSED (would degrade + warn): {}",
                m.message()
            )
        }
        FloorDecision::Fail(m) => {
            outln!(
                "                MISSED (would refuse to launch): {}",
                m.message()
            )
        }
    }
}

/// Machine-readable key for the CPU-cap enforcement mechanism.
fn cpu_cap_key(m: thegn_core::sandbox_cpucap::CpuCap) -> &'static str {
    use thegn_core::sandbox_cpucap::CpuCap;
    match m {
        CpuCap::ScopeHard => "cgroup-hard",
        CpuCap::NiceSoft => "nice-soft",
        CpuCap::None => "none",
    }
}

/// Report the resolved CPU/memory caps and the enforcement mechanism that would
/// apply on THIS host, so a user can see whether the cap is a hard cgroup
/// ceiling or a soft `nice` fallback (or unset). Detection only.
fn cpu_cap_report(cfg: &Config) {
    let limits = &cfg.sandbox.limits;
    // The hardware count, matching what `set_aggregate_caps` would publish.
    let ncpu = thegn_core::sandbox_cpucap::physical_ncpu();
    let per_pane = limits.cpu.as_deref().filter(|s| !s.trim().is_empty());
    let total = thegn_core::sandbox_cpucap::resolve_cpu_total(
        limits.cpu_total.as_deref().unwrap_or("auto"),
        ncpu,
    );
    let mem_total = limits
        .memory_total
        .as_deref()
        .and_then(thegn_core::sandbox_cpucap::resolve_memory_total);
    let mem_pane = limits.memory.as_deref().filter(|s| !s.trim().is_empty());
    if per_pane.is_none() && total.is_none() && mem_total.is_none() && mem_pane.is_none() {
        outln!("  cpu cap       (unset)");
        return;
    }
    let mech = thegn_core::sandbox_cpucap::detect_cpu_cap();
    let mut parts = Vec::new();
    if let Some(c) = per_pane {
        parts.push(format!("{c} cores/pane"));
    }
    if let Some(q) = &total {
        parts.push(format!("{q} total"));
    }
    if parts.is_empty() {
        outln!("  cpu cap       (unset)");
    } else {
        // `label_on`, not `label`: the mechanism can be genuinely *detected*
        // and genuinely unable to reach a pane on this OS. Reporting the probe
        // rather than the outcome is what had macOS claiming a `nice` cap that
        // never applies to anything.
        outln!(
            "  cpu cap       {}  ({})",
            parts.join(" · "),
            mech.label_on(thegn_core::sandbox_backend::host_os())
        );
    }
    // Memory is reported separately because the two halves mean different
    // things: per-pane is a hard `MemoryMax` (exceed it and the pane's tree is
    // OOM-killed), aggregate is a `MemoryHigh` watermark (exceed it and the
    // slice is throttled and reclaimed, never killed).
    let mut mem = Vec::new();
    if let Some(m) = mem_pane {
        mem.push(format!("{m}/pane hard"));
    }
    if let Some(m) = &mem_total {
        mem.push(format!("{m} total (high-water)"));
    }
    if !mem.is_empty() {
        outln!("  mem cap       {}", mem.join(" · "));
    }
    slice_live_report(total.as_deref(), mem_total.as_deref());
}

/// Configured-vs-live for the shared slice, plus a drift flag.
///
/// The config says what this thegn *would* publish; the slice says what is
/// actually bounding every pane, terminal and gate build right now. They came
/// apart in practice — `thegn.slice` is a single user-level unit and
/// `set-property` is last-writer-wins across every thegn on the session, so a
/// stale or nested writer could leave the live ceiling far below the configured
/// one with nothing in the config to show for it. Best-effort: no systemd, no
/// `systemctl`, or no such unit yet prints one honest line and never fails.
fn slice_live_report(configured_cpu: Option<&str>, configured_mem: Option<&str>) {
    let unit = thegn_core::sandbox_cpucap::CPU_SLICE;
    let Some(live) = live_slice_caps() else {
        outln!("  slice live    (not readable — no systemd user manager)");
        return;
    };
    let shown = |v: Option<&str>| v.unwrap_or("unset").to_string();
    outln!(
        "  slice live    {unit}: CPUQuota={} CPUWeight={} MemoryHigh={}",
        shown(live.cpu_quota.as_deref()),
        shown(live.cpu_weight.as_deref()),
        shown(live.memory_high.as_deref()),
    );
    let mut drift = Vec::new();
    if live.cpu_quota.as_deref() != configured_cpu {
        drift.push(format!(
            "CPUQuota configured {} vs live {}",
            shown(configured_cpu),
            shown(live.cpu_quota.as_deref())
        ));
    }
    // Compared as BYTES: "56G" and "60129542144" are the same cap, and string
    // comparison flagged every correctly-applied one as drift.
    let as_bytes = |v: Option<&str>| v.and_then(thegn_core::sandbox_cpucap::mem_bytes);
    if as_bytes(live.memory_high.as_deref()) != as_bytes(configured_mem) {
        drift.push(format!(
            "MemoryHigh configured {} vs live {}",
            shown(configured_mem),
            shown(live.memory_high.as_deref())
        ));
    }
    if drift.is_empty() {
        return;
    }
    outln!("                DRIFT: {}", drift.join("; "));
    if thegn_core::sandbox_cpucap::inside_thegn_slice() {
        outln!(
            "                (this thegn is nested inside {unit} — it inherits the \
             ceiling and never publishes, so the live value is another instance's)"
        );
    } else {
        outln!(
            "                the live value wins until something republishes; \
             restart thegn to apply the configured one"
        );
    }
}

/// The pinned-vs-installed state phrase for a managed tool, given its resolution.
fn tool_version_state(tool: &ManagedTool, res: &Resolution) -> String {
    match res {
        Resolution::Managed { current: true, .. } => format!("pinned {}, current", tool.version),
        Resolution::Managed { .. } if tool.bin_path().exists() => {
            format!("pinned {}, installed differs", tool.version)
        }
        Resolution::Managed { .. } => format!("pinned {}, not installed", tool.version),
        _ => format!("external (managed pin {} bypassed)", tool.version),
    }
}

/// Report each known managed tool: the tier that resolves it (override / PATH /
/// managed), its path, and the pinned-vs-installed state — so a user can see
/// whether a tool is overridden, found on PATH, or managed, and if the managed
/// copy is current. Detection only; resolves via config override + PATH.
fn managed_tools_report(cfg: &Config) {
    outln!("Managed tools ([managed_tools])");
    for tool in crate::managed_tool::known() {
        let over = cfg.managed_tools.get(&tool.name);
        let res = tool.resolve(over, thegn_core::util::which_path);
        outln!(
            "  {:<10} {:<9} {}",
            tool.name,
            res.tier(),
            tool_version_state(&tool, &res)
        );
        outln!("             {}", res.path());
        // BugStalker is Linux-x86-64-only; flag the gate so a "not installed"
        // row on an unsupported host isn't read as merely "run setup".
        if tool.name == "bugstalker"
            && let Some(reason) = thegn_core::debug::unsupported_reason()
        {
            outln!("             note: {reason}");
        }
    }
}

fn managed_tools_json(cfg: &Config) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = crate::managed_tool::known()
        .into_iter()
        .map(|tool| {
            let over = cfg.managed_tools.get(&tool.name);
            let res = tool.resolve(over, thegn_core::util::which_path);
            let is_bugstalker = tool.name == "bugstalker";
            let mut report = serde_json::json!({
                "name": tool.name,
                "tier": res.tier(),
                "path": res.path(),
                "pinned": tool.version,
                "current": matches!(res, Resolution::Managed { current: true, .. }),
            });
            if is_bugstalker {
                report["platform_supported"] =
                    serde_json::json!(thegn_core::debug::platform_supported());
                report["platform_note"] =
                    serde_json::json!(thegn_core::debug::unsupported_reason());
            }
            report
        })
        .collect();
    serde_json::Value::Array(tools)
}

/// Human-readable name for the resolved connectivity state.
fn connectivity_str(c: thegn_core::connectivity::Connectivity) -> &'static str {
    use thegn_core::connectivity::Connectivity;
    match c {
        Connectivity::Online => "online",
        Connectivity::Offline => "offline",
        Connectivity::Unknown => "unknown (no probe yet)",
    }
}

fn network_report(cfg: &Config) {
    // `cfg` is post-processed, so the forced mode is already installed into the
    // holder; `current()` reflects `[network] mode` here.
    outln!("Network / offline ([network])");
    outln!("  mode          {}", cfg.network.mode.as_str());
    outln!(
        "  resolved      {}",
        connectivity_str(thegn_core::connectivity::current())
    );
    outln!(
        "  offline after {} consecutive failures",
        cfg.network.offline_after_failures
    );
    outln!(
        "  recovery probe every {}s while offline",
        cfg.network.recovery_probe_secs
    );
    outln!("  when offline  pause PR/CI/issue refreshes + skip network MCPs (caches served stale)");
}

fn network_json(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "mode": cfg.network.mode.as_str(),
        "resolved": connectivity_str(thegn_core::connectivity::current()),
        "offline_after_failures": cfg.network.offline_after_failures,
        "recovery_probe_secs": cfg.network.recovery_probe_secs,
    })
}

/// Report user-declared MCP servers and their capability grants (detection
/// only; grants gate acquisition in `thegn mcp install`).
fn mcp_servers_report(cfg: &Config) {
    outln!("MCP servers ([mcp_servers])");
    if cfg.mcp_servers.is_empty() {
        outln!("  (none declared)");
        return;
    }
    for (name, srv) in &cfg.mcp_servers {
        let cmd = thegn_core::mcp::config::launch_argv(srv).join(" ");
        outln!("  {name:<12} {cmd}");
        if srv.grants.is_empty() {
            outln!("               grants: none (acquisition refused)");
        } else {
            let gs: Vec<String> = srv
                .grants
                .iter()
                .map(|g| format!("{}={}", g.kind, g.scope))
                .collect();
            outln!("               grants: {}", gs.join(", "));
        }
    }
}

fn mcp_servers_json(cfg: &Config) -> serde_json::Value {
    let servers: Vec<serde_json::Value> = cfg
        .mcp_servers
        .iter()
        .map(|(name, srv)| {
            serde_json::json!({
                "name": name,
                "command": thegn_core::mcp::config::launch_argv(srv),
                "grants": srv.grants.iter().map(|g| serde_json::json!({
                    "kind": g.kind, "scope": g.scope,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::Value::Array(servers)
}

/// The label for a registry entry's key origin.
fn lsp_origin(builtin: bool) -> &'static str {
    if builtin { "built-in" } else { "user" }
}

/// One human resolution phrase for a registry entry's command.
fn lsp_resolution_str(r: &thegn_svc::lsp::Resolution) -> String {
    use thegn_svc::lsp::Resolution;
    match r {
        Resolution::Disabled => "disabled (command = \"\")".to_string(),
        Resolution::Ready(cmd) => format!("{cmd} (ready)"),
        Resolution::Missing(cmd) => format!("{cmd} (missing)"),
    }
}

/// Report the LSP registry: every built-in and user-declared server, the
/// extensions it claims, its resolved command or `missing`, and whether it is
/// masked. Existence-only — no server is spawned (launching arbitrary servers
/// from `doctor` would be a surprising side effect). The house rule that every
/// backend seam carries a probe, applied to language servers.
fn lsp_report(cfg: &Config) {
    use thegn_svc::lsp::Registry;
    outln!("LSP servers ([lsp] + [[lsp.servers]])");
    outln!("  enabled       {}", yn(cfg.lsp.enabled));
    if !cfg.lsp.enabled {
        outln!("  note          [lsp] enabled = false — every server below is masked");
    }
    let reg = Registry::build(&cfg.lsp.servers);
    for e in reg.entries() {
        let exts = if e.extensions.is_empty() {
            "(no extensions)".to_string()
        } else {
            e.extensions
                .iter()
                .map(|x| format!(".{x}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let res = reg
            .describe(&e.key)
            .map(|r| lsp_resolution_str(&r))
            .unwrap_or_default();
        outln!(
            "  {:<13} {:<8} {exts} → {res}",
            e.key,
            lsp_origin(e.builtin)
        );
    }
    // Trust: a worktree-local `.thegn.*` may not inject a server command (it runs
    // on first use). Such entries are ignored; surface why if one is present.
    if let Ok(cwd) = std::env::current_dir()
        && let Some(notice) = thegn_core::lsp_registry::repo_overlay_lsp_notice(&cwd)
    {
        outln!("  trust         {notice}");
    }
}

fn lsp_json(cfg: &Config) -> serde_json::Value {
    use thegn_svc::lsp::{Registry, Resolution};
    let reg = Registry::build(&cfg.lsp.servers);
    let servers: Vec<serde_json::Value> = reg
        .entries()
        .iter()
        .map(|e| {
            let (state, command) = match reg.describe(&e.key) {
                Some(Resolution::Ready(c)) => ("ready", Some(c)),
                Some(Resolution::Missing(c)) => ("missing", Some(c)),
                Some(Resolution::Disabled) | None => ("disabled", None),
            };
            serde_json::json!({
                "key": e.key,
                "extensions": e.extensions,
                "language_id": e.language_id,
                "builtin": e.builtin,
                "command": command,
                "state": state,
                // Masked when the master switch is off, regardless of resolution.
                "masked": !cfg.lsp.enabled,
            })
        })
        .collect();
    serde_json::json!({
        "enabled": cfg.lsp.enabled,
        "hover": cfg.lsp.hover,
        "servers": servers,
    })
}

fn caps_json(c: &TermCaps) -> serde_json::Value {
    serde_json::json!({
        "color": color_str(c.color),
        "glyphs": unicode_str(c.unicode),
        "undercurl": c.undercurl,
        "mouse": c.mouse,
        "osc52": c.osc52,
        "sync_output": c.sync_output,
    })
}

/// A one-line human verdict: what's full vs degraded.
fn summary(c: &TermCaps) -> String {
    let mut on = Vec::new();
    let mut degraded = Vec::new();
    match c.color {
        ColorDepth::Truecolor => on.push("truecolor"),
        ColorDepth::Ansi256 => degraded.push("256-color"),
        ColorDepth::Ansi16 => degraded.push("16-color"),
        ColorDepth::None => degraded.push("no color"),
    }
    match c.unicode {
        UnicodeLevel::Full | UnicodeLevel::Basic => on.push("Unicode glyphs"),
        UnicodeLevel::Ascii => degraded.push("ASCII glyphs"),
    }
    if c.undercurl {
        on.push("undercurl");
    } else {
        degraded.push("plain underline");
    }
    if !c.mouse {
        degraded.push("no mouse");
    }
    let on = if on.is_empty() {
        "nothing".into()
    } else {
        on.join(", ")
    };
    if degraded.is_empty() {
        format!("full fidelity: {on}")
    } else {
        format!("enabled: {on} | degraded: {}", degraded.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_does_not_panic_on_default_config() {
        let cfg = Config::default();
        assert!(run(&cfg, false).is_ok());
        assert!(run(&cfg, true).is_ok());
    }

    /// THE-70. The three states must read differently — "unknown" in
    /// particular must never be phrased as a failure, because thegn assumes the
    /// chords work unless the terminal said otherwise.
    #[test]
    fn keyboard_states_are_distinct_and_unknown_is_not_a_failure() {
        let (ok, broken, unknown) = (
            keyboard_str(Some(true)),
            keyboard_str(Some(false)),
            keyboard_str(None),
        );
        assert_ne!(ok, broken);
        assert_ne!(ok, unknown);
        assert_ne!(broken, unknown);
        assert!(ok.contains("modifyOtherKeys=2"));
        assert!(broken.contains("cannot reach thegn"));
        assert!(unknown.contains("assuming supported"));
    }

    /// A broken row is only useful with a fix attached, and the fix differs:
    /// inside tmux it is a tmux option, outside it is the terminal or a rebind.
    #[test]
    fn keyboard_remedy_is_actionable_in_both_environments() {
        let tmux = keyboard_remedy(true).join("\n");
        assert!(tmux.contains("extended-keys on"));
        assert!(tmux.contains("extkeys"));

        let plain = keyboard_remedy(false).join("\n");
        assert!(plain.contains("modifyOtherKeys level 2"));
        // The rebind escape hatch (design D1: the family stays on Ctrl+<digit>
        // BECAUSE it is rebindable) must name ids `Action::from_key` parses.
        assert!(plain.contains("summon-workspace-1"));
        assert!(plain.contains("summon-pin-1"));
        assert_eq!(
            crate::keymap::Action::from_key("summon-workspace-1"),
            Some(crate::keymap::Action::SummonWorkspace(1)),
        );
        assert_eq!(
            crate::keymap::Action::from_key("summon-pin-1"),
            Some(crate::keymap::Action::SummonPin(1)),
        );
    }

    /// `--json` carries the state so a bug report can be pasted rather than
    /// described. `null` when the probe was skipped — the same "unknown".
    #[test]
    fn doctor_json_exposes_the_keyboard_state() {
        let v = doctor_json(&Config::default());
        let kb = &v["keyboard"];
        assert!(kb.is_object(), "keyboard key missing: {v}");
        for k in [
            "modify_other_keys",
            "kitty_keyboard",
            "ctrl_digits_reportable",
        ] {
            assert!(kb.get(k).is_some(), "keyboard.{k} missing: {kb}");
        }
        // Tests never own a tty, so the probe is skipped and every field is
        // null — which is exactly the "unknown ⇒ assume it works" state.
        assert!(kb["ctrl_digits_reportable"].is_null());
    }

    #[test]
    fn lsp_resolution_phrases_are_distinct() {
        use thegn_svc::lsp::Resolution;
        assert_eq!(
            lsp_resolution_str(&Resolution::Ready("gopls".into())),
            "gopls (ready)"
        );
        assert_eq!(
            lsp_resolution_str(&Resolution::Missing("zls".into())),
            "zls (missing)"
        );
        assert_eq!(
            lsp_resolution_str(&Resolution::Disabled),
            "disabled (command = \"\")"
        );
        assert_eq!(lsp_origin(true), "built-in");
        assert_eq!(lsp_origin(false), "user");
    }

    #[test]
    fn lsp_json_lists_builtins_and_marks_masked_when_disabled() {
        let mut cfg = Config::default();
        cfg.lsp.enabled = false;
        cfg.lsp.servers = vec![thegn_core::config::LspServerConfig {
            lang: "zig".into(),
            command: "zls".into(),
            args: vec![],
            extensions: vec!["zig".into()],
            language_id: None,
        }];
        let v = lsp_json(&cfg);
        assert_eq!(v["enabled"], serde_json::json!(false));
        let servers = v["servers"].as_array().unwrap();
        // 6 built-ins + the user zig entry.
        assert_eq!(servers.len(), 7);
        assert!(
            servers
                .iter()
                .all(|s| s["masked"] == serde_json::json!(true))
        );
        let zig = servers.iter().find(|s| s["key"] == "zig").unwrap();
        assert_eq!(zig["builtin"], serde_json::json!(false));
        assert_eq!(zig["extensions"], serde_json::json!(["zig"]));
    }

    #[test]
    fn option_as_alt_hint_names_the_setting_for_each_known_terminal() {
        // Every terminal in `docs/help/terminal-compatibility.md` must be
        // answerable here, INCLUDING Terminal.app — the default on every Mac and
        // the `.app` bundle's guaranteed fallback, which the help table omitted.
        for (prog, needle) in [
            ("Apple_Terminal", "Use Option as Meta key"),
            ("iTerm.app", "Esc+"),
            ("ghostty", "macos-option-as-alt"),
            ("WezTerm", "send_composed_key"),
            ("Alacritty", "option_as_alt"),
            ("kitty", "macos_option_as_alt"),
        ] {
            let hint = option_as_alt_hint(Some(prog))
                .unwrap_or_else(|| panic!("no Option-as-Alt hint for {prog}"));
            assert!(hint.contains(needle), "{prog}: {hint}");
        }
        // An unknown or absent terminal yields no hint, so the caller falls back
        // to generic advice rather than naming a setting that doesn't exist.
        assert_eq!(option_as_alt_hint(None), None);
        assert_eq!(option_as_alt_hint(Some("some-new-terminal")), None);
    }

    #[test]
    fn home_layer_report_no_panic_and_json_includes_strategy() {
        // Default config: report runs without panicking and the JSON carries the
        // personal-shell strategy + portable-only flag.
        let cfg = Config::default();
        home_layer_report(&cfg);
        let j = home_json(&cfg);
        assert_eq!(j["strategy"], "portable");
        assert_eq!(j["portable_dotfiles_only"], true);
        assert!(j["env_overrides"].is_object());
        // sandbox_json embeds it too.
        assert_eq!(sandbox_json(&cfg)["home"]["strategy"], "portable");
    }

    #[test]
    fn summary_flags_degraded_terminal() {
        let caps = TermCaps {
            color: ColorDepth::None,
            unicode: UnicodeLevel::Ascii,
            undercurl: false,
            mouse: false,
            osc52: true,
            sync_output: false,
        };
        let s = summary(&caps);
        assert!(s.contains("degraded"), "{s}");
        assert!(s.contains("ASCII glyphs"), "{s}");
        assert!(s.contains("no color"), "{s}");
    }

    #[test]
    fn summary_reports_full_fidelity() {
        let s = summary(&TermCaps::FULL);
        assert!(s.starts_with("full fidelity"), "{s}");
    }

    #[test]
    fn isolation_of_resolves_known_backends() {
        use thegn_core::sandbox_backend::HostOs;
        // Pinned per-OS so this asserts what the classifier does, not what the
        // machine running the suite happens to be.
        let isolation_of = |b, rt| isolation_of_on(b, rt, HostOs::Linux);
        assert_eq!(
            isolation_of("bwrap", None),
            Some(IsolationClass::SharedKernel)
        );
        assert_eq!(
            isolation_of("podman", None),
            Some(IsolationClass::SharedKernel)
        );
        // …and on a Mac the same backend is behind a VM.
        assert_eq!(
            isolation_of_on("podman", None, HostOs::MacOs),
            Some(IsolationClass::GuestKernel)
        );
        assert_eq!(
            isolation_of("host", None),
            Some(IsolationClass::HostProcess)
        );
        assert_eq!(isolation_of("not-a-backend", None), None);
        // A stronger OCI runtime raises the reported class for OCI backends…
        assert_eq!(
            isolation_of("podman", Some("runsc")),
            Some(IsolationClass::UserspaceKernel)
        );
        assert_eq!(
            isolation_of("podman", Some("krun")),
            Some(IsolationClass::GuestKernel)
        );
        // …but a non-OCI backend ignores it, and runc/crun stay shared-kernel.
        assert_eq!(
            isolation_of("bwrap", Some("krun")),
            Some(IsolationClass::SharedKernel)
        );
        assert_eq!(
            isolation_of("podman", Some("crun")),
            Some(IsolationClass::SharedKernel)
        );
    }

    #[test]
    fn profile_policy_describes_sealed_lockdown() {
        let p = profile_policy(SandboxProfile::Sealed);
        assert!(p.contains("network=none"), "{p}");
        assert!(p.contains("drop ALL"), "{p}");
        // The default hardened preset leaves caps at runtime defaults.
        let h = profile_policy(SandboxProfile::Hardened);
        assert!(h.contains("runtime default"), "{h}");
    }

    #[test]
    fn managed_tools_json_reports_bs_and_honors_override() {
        // Default config: bugstalker is a managed tool, resolved to the managed
        // tier (nothing on PATH in the test env, no override) and reported.
        let cfg = Config::default();
        let tools = managed_tools_json(&cfg);
        let arr = tools.as_array().expect("array");
        let bs = arr
            .iter()
            .find(|t| t["name"] == "bugstalker")
            .expect("bugstalker reported");
        assert_eq!(bs["pinned"], thegn_core::debug::bs_tool().version);
        assert_eq!(
            bs["platform_supported"],
            thegn_core::debug::platform_supported()
        );
        assert_eq!(
            bs["platform_note"],
            serde_json::to_value(thegn_core::debug::unsupported_reason()).unwrap()
        );
        // The JSON formatter consumes the same pure gate used by the core
        // debugger policy; exercise both sides without changing the host.
        assert!(thegn_core::debug::bs_supported(
            thegn_core::managed_tool::Os::Linux,
            thegn_core::managed_tool::Arch::X64,
        ));
        assert!(!thegn_core::debug::bs_supported(
            thegn_core::managed_tool::Os::Macos,
            thegn_core::managed_tool::Arch::X64,
        ));

        // A user override (as parsed from `[managed_tools.bugstalker]`) wins
        // the tier.
        let mut cfg = Config::default();
        cfg.managed_tools.insert(
            "bugstalker".to_string(),
            thegn_core::managed_tool::ToolOverride {
                path: "/opt/custom/bs".into(),
                args: vec![],
            },
        );
        let arr = managed_tools_json(&cfg);
        let bs = arr
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "bugstalker")
            .unwrap()
            .clone();
        assert_eq!(bs["tier"], "override");
        assert_eq!(bs["path"], "/opt/custom/bs");
        // The report runs without panicking too.
        managed_tools_report(&cfg);
    }

    #[test]
    fn managed_tools_override_parses_from_toml() {
        // `[managed_tools.bs]` layers into Config like the other keyed maps.
        let toml = r#"
[managed_tools.bs]
path = "/usr/local/bin/bs"
args = ["--verbose"]
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let over = cfg.managed_tools.get("bs").expect("override present");
        assert_eq!(over.path, "/usr/local/bin/bs");
        assert_eq!(over.args, vec!["--verbose".to_string()]);
    }

    #[test]
    fn core_deps_json_reports_git_and_gh_without_panicking() {
        // Never panics regardless of whether git/gh are installed; the shape is
        // always present/path/(version|authenticated) for both binaries.
        let v = core_deps_json();
        assert!(v["git"]["present"].is_boolean());
        assert!(v["gh"]["present"].is_boolean());
        // `present` and `path.is_some()` agree.
        assert_eq!(
            v["git"]["present"].as_bool().unwrap(),
            !v["git"]["path"].is_null()
        );
        assert_eq!(
            v["gh"]["present"].as_bool().unwrap(),
            !v["gh"]["path"].is_null()
        );
        // The human report also runs without panicking.
        core_deps_report();
    }

    #[test]
    fn sandbox_json_is_well_formed() {
        let v = sandbox_json(&Config::default());
        assert!(v.get("enabled").is_some());
        assert!(v.get("candidates").unwrap().is_array());
        assert!(v.get("shell_profile").unwrap().get("policy").is_some());
    }

    #[test]
    fn source_control_posture_reports_without_panicking() {
        // Default config: sign_commits is off, so NO signing probe runs and the
        // JSON carries `signing: null`. Both the human and JSON paths run clean.
        let v = source_control_json(&Config::default());
        assert!(v.get("merge_tree_floor").is_some());
        assert_eq!(v["land_strategy"], "merge");
        assert_eq!(v["sign_commits"], false);
        assert!(v["signing"].is_null(), "no probe when signing is off");
        assert!(v["custom_merge_drivers"].is_array());
        source_control_report(&Config::default());
    }

    #[test]
    fn remote_sandbox_probe_is_empty_without_ssh_hosts() {
        // Default config has no [host.*] — the live-probe surface is an empty
        // object and the report doesn't ssh anywhere (so the tests never dial).
        let cfg = Config::default();
        assert!(remote_ssh_placements(&cfg).is_empty());
        assert_eq!(remote_sandbox_json(&cfg), serde_json::json!({}));
        remote_sandbox_report(&cfg);
        assert_eq!(probe_word(RuntimeProbe::Present), "present");
        assert!(probe_word(RuntimeProbe::Unreachable).starts_with("unreachable"));
    }

    #[test]
    fn provider_cache_reports_env_devshell_and_bridge_relation() {
        // No provider envs by default → the surface is an empty object.
        let cfg = Config::default();
        assert!(provider_env_names(&cfg).is_empty());
        assert_eq!(provider_cache_json(&cfg), serde_json::json!({}));

        // A provider env with host_cache + a per-env devshell override is reported;
        // `host_cache_effective` is exactly `host_cache && bridge_present` (we don't
        // assert the absolute bridge state — it depends on the test host).
        use thegn_core::config::{EnvConfig, EnvProviderConfig, SandboxOverlay};
        let mut cfg = Config::default();
        cfg.env.insert(
            "sprites".into(),
            EnvConfig {
                provider: EnvProviderConfig {
                    provider: "sprites".into(),
                    host_cache: true,
                    ..Default::default()
                },
                sandbox: SandboxOverlay {
                    devshell: Some("sprite-full".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let v = provider_cache_json(&cfg);
        let s = &v["sprites"];
        assert_eq!(s["provider"], "sprites");
        assert_eq!(s["devshell"], "sprite-full");
        assert_eq!(s["host_cache"], true);
        let bridge = s["bridge_present"].as_bool().unwrap();
        assert_eq!(s["host_cache_effective"], bridge);
        // env_devshell falls back to the global default when no override is set.
        let plain = EnvConfig {
            provider: EnvProviderConfig {
                provider: "fly".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(env_devshell(&cfg, &plain), "default");
        // The human report runs without panicking.
        provider_cache_report(&cfg);
    }
}
