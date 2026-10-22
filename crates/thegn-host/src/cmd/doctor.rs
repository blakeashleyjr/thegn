//! `thegn doctor` — report the detected terminal capabilities and the
//! feature degradation that follows from them. The manual test surface for the
//! whole terminal-compatibility layer: it shows the raw environment, what
//! `thegn_core::termcaps::detect` makes of it, the effective `[theme]` modes,
//! and the final resolved capabilities (after config overrides) — so you can
//! confirm what a given terminal gets without launching the compositor.

use anyhow::Result;
use thegn_core::capabilities::{Capabilities, IsolationClass};
use thegn_core::config::{Config, FileAccess, SandboxProfile};
use thegn_core::db::Db;
use thegn_core::hooks::HookEvent;
use thegn_core::managed_tool::{ManagedTool, Resolution};
use thegn_core::outln;
use thegn_core::placement::{Placement, RuntimeProbe};
use thegn_core::sandbox::Backend;
use thegn_core::seam::Kind as _;
use thegn_core::store::HostStore;
use thegn_core::termcaps::{ColorDepth, TermCaps, TermEnv, UnicodeLevel};

use super::config_health::ConfigHealth;

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
        out.push("  or rebind in [keybinds]: summon-project-1 … -9 and");
        out.push("  summon-pin-1 … -9, e.g. summon-project-1 = \"Ctrl Alt q\"");
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
    let report = local_backend_report(cfg);
    let (backend, actual) = effective_first_ready(cfg, &report);
    let mut value = sandbox_json_with(cfg, backend, actual);
    value["git_metadata"] = git_metadata_json(&git_metadata_for_config(cfg, backend));
    value
}

fn sandbox_json_with(cfg: &Config, backend: Backend, actual: IsolationClass) -> serde_json::Value {
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
        "isolation_floor": thegn_core::sandbox_floor::floor_in_force(&cfg.sandbox, false).as_str(),
        "isolation_floor_source": if cfg.sandbox.isolation_floor.is_some() {
            "configured"
        } else {
            "default"
        },
        "on_floor_miss": thegn_core::sandbox_floor::miss_policy_in_force(&cfg.sandbox).as_str(),
        "containment": thegn_core::sandbox_floor::containment_summary(
            cfg.sandbox.enabled,
            actual,
        )
        .as_str(),
        "file_access_enforced": thegn_core::sandbox_floor::file_access_enforced(
            backend,
            cfg.sandbox.file_access,
        ),
        "enforcement_matrix": enforcement_matrix_json(),
        "home": home_json(cfg),
        "network_rules": network_rules_line(&cfg.sandbox, backend),
        "admission_routes": crate::sandbox_admission::routes_json(),
        "isolation": isolation_json(&thegn_core::sandbox_floor::isolation_report(
            &cfg.sandbox,
            backend,
            actual,
            actual,
        )),
    })
}

/// Serialize the shared isolation model without changing the legacy doctor keys.
fn isolation_json(report: &thegn_core::sandbox_floor::IsolationReport) -> serde_json::Value {
    serde_json::json!({
        "requested": {
            "profile": report.requested.profile.as_str(),
            "network": report.requested.network.as_str(),
            "isolation_floor": report.requested.floor.as_str(),
            "isolation_floor_source": if report.requested.floor_configured {
                "configured"
            } else {
                "default"
            },
            "on_floor_miss": report.requested.on_floor_miss.as_str(),
            "file_access": thegn_core::sandbox_floor::file_access_str(report.requested.file_access),
        },
        "resolved": {
            "backend": report.resolved.backend.label(),
            "class": report.resolved.class.as_str(),
        },
        "enforced": {
            "floor": report.enforced.floor.as_str(),
            "containment": report.enforced.containment.as_str(),
            "file_access": report.enforced.file_access,
        },
    })
}

/// Token-level warning heuristic for harnesses that offer a full-access mode.
/// This does not interpret shell syntax; exact command tokens are used so a
/// mention in a prompt or a longer option name cannot trigger the warning.
fn has_full_access_token(command: &str) -> bool {
    command.split_whitespace().any(|token| {
        let token = token.trim_matches(['\'', '"']);
        let value = token
            .rsplit_once('=')
            .map(|(_, value)| value)
            .unwrap_or(token)
            .trim_matches(['\'', '"']);
        matches!(
            token,
            "--dangerously-skip-permissions"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--yolo"
                | "danger-full-access"
        ) || value == "danger-full-access"
    })
}

fn unsafe_full_access_stages(cfg: &Config, backend: Backend) -> Vec<(String, String)> {
    if cfg.sandbox.enabled && backend != Backend::None {
        return Vec::new();
    }
    cfg.pipeline
        .stages
        .iter()
        .filter_map(|stage| {
            let agent =
                thegn_core::agent_task::effective_agent(cfg, &stage.agent, Some(&stage.name))
                    .ok()?;
            let command = agent.interactive_command().ok()?;
            has_full_access_token(&command).then(|| (stage.name.clone(), agent.name.clone()))
        })
        .collect()
}

fn stage_worker_access_report(cfg: &Config) {
    let report = thegn_core::sandbox_support::support_report(
        &shell_chain(cfg),
        &Placement::Local,
        cfg_oci_runtime(cfg),
    );
    let backend = thegn_core::sandbox_support::first_ready(&report)
        .map(|row| row.backend)
        .unwrap_or(Backend::None);
    for (stage, agent) in unsafe_full_access_stages(cfg, backend) {
        outln!(
            "  UNSAFE        pipeline stage `{stage}` agent `{agent}` has a full-access command token while no containment is resolved (heuristic)"
        );
    }
}

fn devcontainer_json(cfg: &Config) -> serde_json::Value {
    let Some(root) = current_repo_root() else {
        return serde_json::json!({
            "mode": cfg.sandbox.devcontainer.as_str(),
            "repo": null,
            "candidates": [],
            "selected": null,
        });
    };
    // Environment selection and devcontainer discovery are worktree-scoped,
    // not cwd-scoped.  In a linked worktree `root` is the common checkout's
    // root, while `--show-toplevel` is the worktree the user is diagnosing.
    let worktree = current_worktree_root().unwrap_or_else(|| root.clone());
    let db = thegn_core::db::Db::open().ok();
    let approvals = db
        .as_ref()
        .map(|db| crate::handlers::repo_trust::approvals_for(db, &root.to_string_lossy()))
        .unwrap_or_else(thegn_core::config_resolve::Approvals::deny_all);
    let environment = crate::handlers::repo_trust::effective_environment_for_worktree(
        cfg,
        db.as_ref(),
        &root,
        &worktree,
        None,
        &approvals,
    );
    let sandbox = environment.sandbox.clone();
    if sandbox.devcontainer == thegn_core::config::DevcontainerMode::Off {
        return serde_json::json!({
            "mode": "off",
            "repo": root.display().to_string(),
            "candidates": [],
            "selected": null,
            "status": { "variant": "", "state": "off", "reason": "disabled by repo sandbox policy" },
        });
    }
    if !environment.allows_devcontainer() {
        let reason = if sandbox.enabled {
            "effective local environment is explicitly uncontained"
        } else {
            "sandboxing is disabled in the effective environment"
        };
        return serde_json::json!({
            "mode": sandbox.devcontainer.as_str(),
            "repo": root.display().to_string(),
            "candidates": [],
            "selected": null,
            "status": { "variant": "", "state": "off", "reason": reason },
        });
    }
    let selection = thegn_core::devcontainer_select::select_and_parse(
        &worktree,
        Some(&cfg.repo_devcontainer_selector(&root)),
    );
    let candidates: Vec<String> = selection
        .candidates
        .iter()
        .map(|p| {
            thegn_core::devcontainer_select::relative_path(&worktree, p)
                .display()
                .to_string()
        })
        .collect();
    let selected = selection.selected.as_ref().map(|p| {
        thegn_core::devcontainer_select::relative_path(&worktree, p)
            .display()
            .to_string()
    });
    let error = selection.error.as_ref().map(ToString::to_string);
    let probe = crate::devcontainer_provider::probe();
    let provider = serde_json::json!({
        "state": format!("{:?}", probe.state).to_lowercase(),
        "executable": probe.executable.clone(),
        "version": probe.version.clone(),
        "reason": probe.reason.clone(),
    });
    let Some(config) = selection.config.as_ref() else {
        return serde_json::json!({
            "mode": sandbox.devcontainer.as_str(),
            "repo": root.display().to_string(),
            "candidates": candidates,
            "selected": selected,
            "error": error,
            "provider": provider,
        });
    };
    let allowed = sandbox.env_passthrough.clone();
    let firewall = thegn_core::profile::CredentialFirewall::active();
    let local_env = |key: &str| {
        (allowed.iter().any(|allowed_key| allowed_key == key) && firewall.admits(key))
            .then(|| std::env::var(key).ok())
            .flatten()
    };
    let allow_local_env =
        |key: &str| allowed.iter().any(|allowed_key| allowed_key == key) && firewall.admits(key);
    let ctx = thegn_core::devcontainer::SubstCtx {
        local_workspace_folder: worktree.to_string_lossy().into_owned(),
        container_workspace_folder: worktree.to_string_lossy().into_owned(),
        local_env: &local_env,
        container_env: &|_| None,
    };
    let mut folded = sandbox.clone();
    let gated = thegn_core::devcontainer_overlay::apply_gated_with_policy(
        config,
        &mut folded,
        &ctx,
        &worktree.to_string_lossy(),
        &approvals,
        &allow_local_env,
    );
    let inventory = thegn_core::devcontainer::recognized_unapplied(config);
    let honorability = Backend::from_config(folded.backend).map(|backend| {
        format!(
            "{:?}",
            thegn_core::devcontainer_overlay::backend_honorability_for(config, backend)
        )
    });
    let status = crate::devcontainer_provider::status_for_selected(
        config, &selection, &worktree, &sandbox, &approvals, &probe,
    );
    serde_json::json!({
        "mode": sandbox.devcontainer.as_str(),
        "repo": root.display().to_string(),
        "candidates": candidates,
        "selected": selected,
        "error": error,
        "status": { "variant": status.variant, "state": status.state_label(), "reason": status.reason },
        "provider": provider,
        "trust": {
            "approved": inventory.applied,
            "pending": gated.pending.iter().map(|p| p.key.clone()).collect::<Vec<_>>(),
            "refused": inventory.refused,
            "reserved": inventory.reserved,
            "editor_only": inventory.editor_only,
            "unknown": inventory.unknown,
        },
        "backend_honorability": honorability,
    })
}

fn devcontainer_report(cfg: &Config) {
    let report = devcontainer_json(cfg);
    outln!("Devcontainer support");
    outln!(
        "  mode          {}",
        report["mode"].as_str().unwrap_or("unknown")
    );
    if let Some(repo) = report["repo"].as_str() {
        outln!("  repo          {repo}");
    } else {
        outln!("  repo          (not inside a repository)");
    }
    let candidates = report["candidates"].as_array().cloned().unwrap_or_default();
    let candidate_text = candidates
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    outln!(
        "  candidates    {}",
        if candidate_text.is_empty() {
            "(none)"
        } else {
            &candidate_text
        }
    );
    outln!(
        "  selected      {}",
        report["selected"].as_str().unwrap_or("(none)")
    );
    if let Some(error) = report["error"].as_str() {
        outln!("  selection     {error}");
    }
    let provider = &report["provider"];
    outln!(
        "  provider      {}",
        provider["state"].as_str().unwrap_or("unknown")
    );
    if let Some(status) = report["status"]["state"].as_str() {
        outln!(
            "  status        {status} ({})",
            report["status"]["variant"].as_str().unwrap_or("default")
        );
    }
    outln!(
        "  trust         {} pending, {} refused/reserved/unknown",
        report["trust"]["pending"].as_array().map_or(0, Vec::len),
        report["trust"]["refused"].as_array().map_or(0, Vec::len)
            + report["trust"]["reserved"].as_array().map_or(0, Vec::len)
            + report["trust"]["unknown"].as_array().map_or(0, Vec::len)
    );
    if let Some(honor) = report["backend_honorability"].as_str() {
        outln!("  backend       {honor}");
    }
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
        "cpu_scope_note": thegn_core::sandbox_cpucap::detected_scope_skipped_note(),
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
    let xdg_omissions = current_xdg_omissions(cfg)
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path.to_string_lossy(),
                "reason": entry.reason.as_str(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "strategy": cfg.sandbox.home.strategy.as_str(),
        "portable_dotfiles_only": cfg.sandbox.home.portable_dotfiles_only,
        "env_overrides": env_overrides,
        "xdg_carveout_omissions": xdg_omissions,
    })
}

fn current_xdg_omissions(cfg: &Config) -> Vec<thegn_core::sandbox_mounts::XdgOmissionEntry> {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Vec::new();
    };
    let (_, omissions) =
        thegn_core::sandbox_mounts::xdg_carveout_report_at(cfg.sandbox.profile, &home, &|key| {
            std::env::var_os(key)
        });
    omissions
}

fn format_xdg_omissions(omissions: &[thegn_core::sandbox_mounts::XdgOmissionEntry]) -> Vec<String> {
    omissions
        .iter()
        .map(|entry| format!("{} — {}", entry.path.display(), entry.reason.as_str()))
        .collect()
}

/// Every provider the loaded config selects, with its probe (the
/// provider-seams registry). Reserved kinds show up as unavailable with the
/// reason, so "why is CI empty?" has a one-line answer.
fn providers_json(cfg: &Config) -> serde_json::Value {
    serde_json::to_value(thegn_svc::seam::registry::probes(cfg)).unwrap_or_default()
}

fn toolchain_json(cfg: &Config) -> serde_json::Value {
    let worktree = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    let root = thegn_core::repo::main_worktree(&worktree).unwrap_or_else(|| worktree.clone());
    let db = thegn_core::db::Db::open().ok();
    serde_json::to_value(crate::mise_provider::doctor_status(
        cfg,
        &worktree,
        &root,
        db.as_ref(),
    ))
    .unwrap_or_default()
}

fn toolchain_report(cfg: &Config) {
    let report = toolchain_json(cfg);
    outln!("Worktree toolchain (cached, presence-only)");
    for key in ["provider", "tier", "inject", "state", "trust"] {
        outln!("  {key:<13} {}", report[key].as_str().unwrap_or("unknown"));
    }
    let files = report["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    outln!(
        "  files         {}",
        if files.is_empty() { "(none)" } else { &files }
    );
    if let Some(shims) = report["shims"].as_str() {
        outln!("  shims         {shims}");
    }
    if let Some(reason) = report["reason"].as_str() {
        outln!("  reason        {reason}");
    }
    if let Some(version) = report["version"].as_str() {
        outln!("  version       {version}");
    }
    let missing = report["missing_tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    outln!(
        "  missing       {}",
        if missing.is_empty() { "none" } else { &missing }
    );
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
    let sound = crate::notification_sound::SoundRuntime::report(&cfg.notifications.sound);
    let provider = &sound["provider"];
    let availability = provider["availability"]["state"]
        .as_str()
        .unwrap_or("unknown");
    outln!(
        "  {:<9} {:<24} {availability}{}",
        "sound",
        provider["id"].as_str().unwrap_or("none"),
        provider["availability"]["reason"]
            .as_str()
            .map(|reason| format!(" — {reason}"))
            .unwrap_or_default(),
    );
    outln!(
        "  {:<9} {:<24} formats: {}; volume: {}",
        "",
        "",
        provider["caps"]["formats"]
            .as_array()
            .map(|formats| {
                formats
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        provider["caps"]["volume"].as_bool().map_or("unknown", |v| {
            if v { "supported" } else { "unsupported" }
        }),
    );
    outln!(
        "  {:<9} {:<24} pack: {}; entries: {}",
        "",
        "",
        sound["pack"].as_str().unwrap_or("(none)"),
        sound["pack_entries"].as_u64().unwrap_or(0),
    );
    if let Some(reason) = sound["fallback"].as_str() {
        outln!("  {:<9} {:<24} fallback: {reason}", "", "");
    }
}

/// Find hard-linked secret staging files that can retain replaced contents.
fn secret_hard_link_notes(state_thegn: &std::path::Path) -> Vec<String> {
    fn append_directory_notes(dir: &std::path::Path, notes: &mut Vec<String>) {
        match thegn_core::fsperm::hard_linked_files(dir) {
            Ok(files) => {
                for (path, extra) in files {
                    if let Some(note) = thegn_core::fsperm::hard_link_residue_note(&path, extra) {
                        notes.push(note);
                    }
                }
            }
            Err(error) => notes.push(format!("could not inspect {}: {error}", dir.display())),
        }
    }

    let mut notes = Vec::new();
    append_directory_notes(&state_thegn.join("sandbox-env"), &mut notes);

    let vpn_dir = state_thegn.join("vpn");
    match std::fs::read_dir(&vpn_dir) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let dir = entry.path();
                        match std::fs::symlink_metadata(&dir) {
                            Ok(metadata)
                                if metadata.file_type().is_dir()
                                    && !metadata.file_type().is_symlink() =>
                            {
                                append_directory_notes(&dir, &mut notes);
                            }
                            Ok(_) => {}
                            Err(error) => {
                                notes.push(format!("could not inspect {}: {error}", dir.display()))
                            }
                        }
                    }
                    Err(error) => {
                        notes.push(format!("could not inspect {}: {error}", vpn_dir.display()))
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => notes.push(format!("could not inspect {}: {error}", vpn_dir.display())),
    }
    notes.sort();
    notes
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
    } else {
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
    for note in secret_hard_link_notes(&thegn_core::util::xdg_state_home().join("thegn")) {
        outln!("  note: {note}");
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
    let firewall = thegn_core::profile::CredentialFirewall::active();
    if firewall.is_named() {
        let profile_name = thegn_core::profile::name();
        let effective_pass = cfg
            .profiles
            .get(&profile_name)
            .and_then(|profile| profile.sandbox.env_passthrough.clone())
            .unwrap_or_else(|| pass.to_vec());
        let denied = firewall.denied(&effective_pass);
        let shares = thegn_core::profile::profile_shares(cfg, &profile_name);
        outln!("  profile firewall ({profile_name}): ambient credentials denied by default");
        let denied_names = if denied.is_empty() {
            "(none)".to_owned()
        } else {
            denied.join(", ")
        };
        let shared_names = if shares.is_empty() {
            "(none)".to_owned()
        } else {
            shares.join(", ")
        };
        outln!("    denied  {denied_names}");
        outln!("    shared  {shared_names}");
        outln!("    share one with [profiles.{profile_name}.sandbox] env_passthrough = [...]");
    }
    outln!("  note: `/run/user` is not a default mount (keyring/agent unreachable);");
    outln!("        add it to [sandbox] mounts only if a pane needs the session bus.");
}

/// One-line control-surface coverage summary (cells implemented / declared,
/// gap count). The full per-surface ledger is `thegn api coverage`.
fn local_control_security_json(cfg: &Config) -> serde_json::Value {
    local_control_security_json_with_legacy_state(cfg, None)
}

fn local_control_security_json_with_legacy_state(
    cfg: &Config,
    legacy_state_override: Option<&str>,
) -> serde_json::Value {
    let socket = crate::daemon::socket_path(&cfg.daemon);
    let endpoint = thegn_svc::ipc::IpcEndpoint::for_socket_path(&socket);
    let legacy_path = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    let legacy_state =
        legacy_state_override.unwrap_or_else(|| match crate::daemon::legacy_socket_state() {
            crate::daemon::LegacySocketState::Live => "live",
            crate::daemon::LegacySocketState::Stale => "stale",
            crate::daemon::LegacySocketState::Absent => "absent",
        });
    let legacy_socket = serde_json::json!({
        "path": legacy_path.display().to_string(),
        "state": legacy_state,
    });
    if !cfg.serve.local_admin {
        return serde_json::json!({
            "endpoint": endpoint.display(),
            "legacy_socket": legacy_socket,
            "auth": "token-required",
            "peer_identity": "not-used",
            "hardening": "not-required",
        });
    }
    let security = crate::platform::local_control_security(&socket);
    serde_json::json!({
        "endpoint": endpoint.display(),
        "legacy_socket": legacy_socket,
        "auth": security.auth,
        "peer_identity": security.peer_identity,
        "hardening": security.hardening,
        "error": security.error,
    })
}

fn remote_control_transport_json(cfg: &Config) -> serde_json::Value {
    match cfg.serve.resolve_transport(None, false) {
        Ok(policy) => serde_json::json!({
            "status": policy.exposure.as_str(),
            "topology": cfg.serve.topology.as_str(),
            "backend_bind": policy.bind.to_string(),
            "advertised_origin": policy.advertised_origin(policy.bind.port()),
            "http": policy.http_scheme(),
            "websocket": policy.websocket_scheme(),
            "grpc": policy.grpc_scheme(),
            "scoped_tokens_required": true,
            "unsafe_opt_in": cfg.serve.unsafe_allow_plaintext_non_loopback,
        }),
        Err(error) => serde_json::json!({
            "status": "invalid",
            "topology": cfg.serve.topology.as_str(),
            "backend_bind": cfg.serve.bind,
            "scoped_tokens_required": true,
            "unsafe_opt_in": cfg.serve.unsafe_allow_plaintext_non_loopback,
            "error": error,
        }),
    }
}

fn control_surface_report(cfg: &Config) {
    let report = crate::cmd::api::coverage_report();
    let ledgers = &report.surfaces;
    let implemented: usize = ledgers.iter().map(|l| l.implemented + l.stub).sum();
    let declared: usize = ledgers.iter().map(|l| l.declared).sum();
    let stubs: usize = ledgers.iter().map(|l| l.stub).sum();
    let gaps: usize = ledgers.iter().map(|l| l.excused).sum();
    outln!("Control-surface coverage (see `thegn api coverage`)");
    outln!("  revision     {}", report.revision);
    outln!("  schema_version {}", report.schema_version);
    outln!(
        "  cells         {implemented}/{declared} implemented ({stubs} stub, {gaps} excused gap{})",
        if gaps == 1 { "" } else { "s" }
    );
    let local = local_control_security_json(cfg);
    outln!(
        "  local auth    {}",
        local["auth"].as_str().unwrap_or("unknown")
    );
    outln!(
        "  peer identity {}",
        local["peer_identity"].as_str().unwrap_or("unknown")
    );
    match local["hardening"].as_str().unwrap_or("unknown") {
        "failed" => outln!(
            "  hardening     FAILED: {}",
            local["error"].as_str().unwrap_or("unknown error")
        ),
        status => outln!("  hardening     {status}"),
    }
    let remote = remote_control_transport_json(cfg);
    outln!(
        "  remote        {} ({})",
        remote["status"].as_str().unwrap_or("invalid"),
        remote["topology"].as_str().unwrap_or("unknown")
    );
    if remote["status"] == "invalid" {
        outln!(
            "  remediation   {}",
            remote["error"].as_str().unwrap_or("invalid [serve] config")
        );
    } else {
        outln!(
            "  advertised    {} · WebSocket {} · gRPC {}",
            remote["advertised_origin"].as_str().unwrap_or("unknown"),
            remote["websocket"].as_str().unwrap_or("unknown"),
            remote["grpc"].as_str().unwrap_or("unknown"),
        );
    }
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
        "iroh_reach": if thegn_svc::iroh_reach::BUILT { "built" } else { "not built" },
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
    outln!(
        "  {:<13} {}",
        "iroh reach",
        if thegn_svc::iroh_reach::BUILT {
            "built in (THEGN_IROH=1 enables it)"
        } else {
            "not built (release and Nix builds include it)"
        }
    );
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
    thegn_metrics::coverage_with_collectors(&snap, monitor_collectors_health())
}

/// Discover the CLI diagnostic collector set once; visible-only readers are
/// deliberately left untouched because their cost belongs to visible panels.
/// This process-wide cache assumes CLI callers (`thegn doctor` and `thegn
/// bundle`) exit after the command. A long-lived caller of `doctor_json*`
/// would retain this health snapshot for the process lifetime.
fn monitor_collectors_health()
-> &'static [thegn_metrics::collect::health::Health; thegn_metrics::collect::COLLECTORS] {
    static HEALTH: std::sync::OnceLock<
        [thegn_metrics::collect::health::Health; thegn_metrics::collect::COLLECTORS],
    > = std::sync::OnceLock::new();
    HEALTH.get_or_init(|| {
        discover_monitor_collectors(
            thegn_metrics::collect::default_collectors(),
            &thegn_metrics::collect::env::Env::default(),
        )
    })
}

fn discover_monitor_collectors(
    mut collectors: Vec<Box<dyn thegn_metrics::collect::Collector>>,
    env: &thegn_metrics::collect::env::Env,
) -> [thegn_metrics::collect::health::Health; thegn_metrics::collect::COLLECTORS] {
    use thegn_metrics::collect::health::{Absent, Health};

    let mut health = [Health::Absent(Absent::NotImplemented); thegn_metrics::collect::COLLECTORS];
    for collector in &mut collectors {
        let id = collector.id();
        if !id.visible_only() {
            health[id.index()] = collector.discover(env);
        }
    }
    for collector in &mut collectors {
        collector.close();
    }
    health
}

fn monitor_collector_health(
    id: thegn_metrics::collect::CollectorId,
    health: &[thegn_metrics::collect::health::Health; thegn_metrics::collect::COLLECTORS],
) -> String {
    use thegn_metrics::collect::health::{Absent, Health};

    if id.visible_only() {
        return "visible-only (not probed)".to_string();
    }
    match health[id.index()] {
        Health::Ok => "ok".to_string(),
        Health::Warming => "warming".to_string(),
        Health::Failing { .. } => "failing".to_string(),
        Health::Absent(Absent::NoPermission) => "denied".to_string(),
        Health::Absent(reason) => format!("absent ({})", reason.to_coverage().word()),
    }
}

fn denied_hint(id: thegn_metrics::collect::CollectorId) -> &'static str {
    use thegn_metrics::collect::CollectorId;

    match id {
        CollectorId::Powercap => {
            "RAPL energy_uj files are root-only (0400); see docs/help/debugging.md for an optional udev rule."
        }
        _ => "This reading needs privileges thegn does not request; it never escalates or prompts.",
    }
}

fn monitor_collectors_json() -> serde_json::Value {
    monitor_collectors_json_for(monitor_collectors_health())
}

fn monitor_collectors_json_for(
    health: &[thegn_metrics::collect::health::Health; thegn_metrics::collect::COLLECTORS],
) -> serde_json::Value {
    use thegn_metrics::collect::{
        CollectorId,
        health::{Absent, Health},
    };

    let mut map = serde_json::Map::new();
    for id in CollectorId::ALL {
        let status = monitor_collector_health(id, health);
        let value = if id.visible_only() {
            serde_json::json!({ "health": status })
        } else {
            match health[id.index()] {
                Health::Absent(Absent::NoPermission) => serde_json::json!({
                    "health": status,
                    "hint": denied_hint(id),
                }),
                Health::Absent(reason) => serde_json::json!({
                    "health": "absent",
                    "reason": reason.to_coverage().word(),
                }),
                _ => serde_json::json!({ "health": status }),
            }
        };
        map.insert(id.slug().to_string(), value);
    }
    serde_json::Value::Object(map)
}

fn monitor_collectors_lines(
    health: &[thegn_metrics::collect::health::Health; thegn_metrics::collect::COLLECTORS],
) -> Vec<String> {
    use thegn_metrics::collect::{
        CollectorId,
        health::{Absent, Health},
    };

    CollectorId::ALL
        .into_iter()
        .flat_map(|id| {
            let status = format!(
                "  {:<16} {}",
                id.slug(),
                monitor_collector_health(id, health)
            );
            if !id.visible_only()
                && matches!(health[id.index()], Health::Absent(Absent::NoPermission))
            {
                vec![status, format!("    {}", denied_hint(id))]
            } else {
                vec![status]
            }
        })
        .collect()
}

fn monitor_collectors_report() {
    outln!("Monitor collectors");
    for line in monitor_collectors_lines(monitor_collectors_health()) {
        outln!("{line}");
    }
}

#[cfg(test)]
#[test]
fn monitor_collector_formatters_cover_every_id_and_health_state() {
    use thegn_metrics::collect::{
        COLLECTORS, CollectorId,
        health::{Absent, Health},
    };

    let mut health = [Health::Warming; COLLECTORS];
    health[CollectorId::Loadavg.index()] = Health::Ok;
    health[CollectorId::Uptime.index()] = Health::Failing {
        since_tick: 1,
        backoff: 2,
        retry_at: 4,
    };
    health[CollectorId::Psi.index()] = Health::Absent(Absent::NoHardware);
    health[CollectorId::CpuStat.index()] = Health::Absent(Absent::NoPermission);
    health[CollectorId::Powercap.index()] = Health::Absent(Absent::NoPermission);

    let expected_status = |id| match id {
        CollectorId::Loadavg => "ok",
        CollectorId::Uptime => "failing",
        CollectorId::Psi => "absent (no such hardware)",
        CollectorId::CpuStat => "denied",
        CollectorId::Powercap => "denied",
        id if id.visible_only() => "visible-only (not probed)",
        _ => "warming",
    };
    for id in CollectorId::ALL {
        let expected_hint = if id == CollectorId::Powercap {
            "RAPL energy_uj files are root-only (0400); see docs/help/debugging.md for an optional udev rule."
        } else {
            "This reading needs privileges thegn does not request; it never escalates or prompts."
        };
        assert_eq!(denied_hint(id), expected_hint);
    }
    let expected_text = std::iter::once("Monitor collectors".to_string())
        .chain(CollectorId::ALL.into_iter().flat_map(|id| {
            let status = format!("  {:<16} {}", id.slug(), expected_status(id));
            if matches!(id, CollectorId::CpuStat | CollectorId::Powercap) {
                vec![status, format!("    {}", denied_hint(id))]
            } else {
                vec![status]
            }
        }))
        .collect::<Vec<_>>()
        .join("\n");
    let actual_text = std::iter::once("Monitor collectors".to_string())
        .chain(monitor_collectors_lines(&health))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(actual_text, expected_text);
    assert!(
        monitor_collectors_lines(&health)
            .iter()
            .any(|line| line.as_str() == "  loadavg          ok")
    );
    let lines = monitor_collectors_lines(&health);
    assert!(
        lines
            .iter()
            .any(|line| { line == &format!("    {}", denied_hint(CollectorId::Powercap)) })
    );
    assert!(
        lines
            .iter()
            .any(|line| { line == &format!("    {}", denied_hint(CollectorId::CpuStat)) })
    );

    let expected_json: serde_json::Map<String, serde_json::Value> = CollectorId::ALL
        .into_iter()
        .map(|id| {
            let value = match id {
                CollectorId::Psi => serde_json::json!({
                    "health": "absent",
                    "reason": "no such hardware",
                }),
                CollectorId::CpuStat | CollectorId::Powercap => serde_json::json!({
                    "health": "denied",
                    "hint": denied_hint(id),
                }),
                _ => serde_json::json!({ "health": expected_status(id) }),
            };
            (id.slug().to_string(), value)
        })
        .collect();
    let actual_json = monitor_collectors_json_for(&health)
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(actual_json, expected_json);
}

#[cfg(test)]
#[test]
fn monitor_collector_discovery_skips_visible_only_and_closes_collectors() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use thegn_metrics::collect::{
        CollectError, Collector, CollectorId, TickCtx, env::Env, frame::MonitorFrame,
        health::Health, schedule::Cadence,
    };

    struct CountingCollector {
        id: CollectorId,
        discovered: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }

    impl Collector for CountingCollector {
        fn id(&self) -> CollectorId {
            self.id
        }
        fn cadence(&self) -> Cadence {
            Cadence::Every(1)
        }
        fn discover(&mut self, _env: &Env) -> Health {
            self.discovered.fetch_add(1, Ordering::SeqCst);
            Health::Ok
        }
        fn sample(
            &mut self,
            _ctx: &TickCtx,
            _frame: &mut MonitorFrame,
        ) -> Result<(), CollectError> {
            Ok(())
        }
        fn close(&mut self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    let normal_discovered = Arc::new(AtomicUsize::new(0));
    let visible_discovered = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(CountingCollector {
            id: CollectorId::Loadavg,
            discovered: Arc::clone(&normal_discovered),
            closed: Arc::clone(&closed),
        }),
        Box::new(CountingCollector {
            id: CollectorId::GpuClients,
            discovered: Arc::clone(&visible_discovered),
            closed: Arc::clone(&closed),
        }),
    ];

    let health = discover_monitor_collectors(collectors, &Env::default());
    assert_eq!(normal_discovered.load(Ordering::SeqCst), 1);
    assert_eq!(visible_discovered.load(Ordering::SeqCst), 0);
    assert_eq!(closed.load(Ordering::SeqCst), 2);
    assert_eq!(health[CollectorId::Loadavg.index()], Health::Ok);
    assert_eq!(
        health[CollectorId::GpuClients.index()],
        Health::Absent(thegn_metrics::collect::health::Absent::NotImplemented)
    );
}

#[cfg(test)]
#[test]
fn doctor_json_exposes_monitor_collector_health_for_every_id() {
    use thegn_metrics::collect::CollectorId;

    let report = doctor_json(&Config::default());
    let collectors = report["monitor_collectors"].as_object().unwrap();
    assert_eq!(collectors.len(), CollectorId::ALL.len());
    for id in CollectorId::ALL {
        let entry = &collectors[id.slug()];
        assert!(entry["health"].is_string(), "{}: {entry}", id.slug());
        assert_eq!(entry.get("reason").is_some(), entry["health"] == "absent");
    }
    assert_eq!(
        collectors[CollectorId::GpuClients.slug()]["health"],
        "visible-only (not probed)"
    );
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

fn medium_word(medium: thegn_metrics::NetMedium) -> &'static str {
    match medium {
        thegn_metrics::NetMedium::Ethernet => "ethernet",
        thegn_metrics::NetMedium::Wifi => "wifi",
        thegn_metrics::NetMedium::Unknown => "generic",
    }
}

fn network_path_json(selection: &thegn_metrics::MediumSelection) -> serde_json::Value {
    serde_json::json!({
        "medium": medium_word(selection.medium),
        "interface": selection.iface.as_deref(),
        "reason": selection.reason.word(),
    })
}

fn network_path_report(selection: &thegn_metrics::MediumSelection) {
    let medium = medium_word(selection.medium);
    if let Some(iface) = &selection.iface {
        outln!(
            "  net path      {medium} via {iface} ({})",
            selection.reason.word()
        );
    } else {
        outln!("  net path      {medium} ({})", selection.reason.word());
    }
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
    network_path_report(&thegn_metrics::probe_net_path());
    disk_health_report(crate::platform::disk_health(
        std::time::Duration::from_secs(10),
    ));
}

fn disk_health_report(report: crate::platform::DiskHealthReport) {
    outln!("Disk health");
    match report {
        crate::platform::DiskHealthReport::Unavailable(reason) => {
            outln!("  health: n/a (udisks2 unavailable): {reason}");
        }
        crate::platform::DiskHealthReport::Degraded { drives, reason } => {
            outln!("  health: partial ({reason})");
            disk_health_drives_report(drives);
        }
        crate::platform::DiskHealthReport::Available(drives) if drives.is_empty() => {
            outln!("  health: n/a (udisks2 unavailable): no drives reported");
        }
        crate::platform::DiskHealthReport::Available(drives) => disk_health_drives_report(drives),
    }
}

fn disk_health_drives_report(drives: Vec<crate::platform::DiskHealth>) {
    for drive in drives {
        let serial = drive.serial_last4.as_deref().unwrap_or("n/a");
        let health = drive.health.as_deref().unwrap_or("n/a");
        let warning = drive.critical_warning.as_ref().map_or_else(
            || "n/a".to_string(),
            |warnings| {
                if warnings.is_empty() {
                    "none".to_string()
                } else {
                    warnings.join(", ")
                }
            },
        );
        outln!(
            "  {} serial …{} health {health} critical-warning {warning}",
            drive.name,
            serial
        );
        outln!(
            "    wear {}% spare {}/{} read {} B written {} B",
            optional_metric(drive.percent_used),
            optional_metric(drive.available_spare),
            optional_metric(drive.spare_threshold),
            optional_metric(drive.data_read_bytes),
            optional_metric(drive.data_written_bytes),
        );
        outln!(
            "    power {} h cycles {} unsafe-shutdowns {} media-errors {} ATA-bad-sectors {} temperature {} °C",
            optional_metric(drive.power_on_hours),
            optional_metric(drive.power_cycles),
            optional_metric(drive.unsafe_shutdowns),
            optional_metric(drive.media_errors),
            optional_metric(drive.ata_bad_sectors),
            drive
                .temperature_c
                .map_or_else(|| "n/a".to_string(), |value| value.to_string()),
        );
    }
}

fn optional_metric(value: Option<u64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |value| value.to_string())
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
            match thegn_core::agent_task::effective_agent(cfg, &a.name, None)
                // A list the harness cannot grant command-scoped refuses the
                // launch (THE-440) — show the entry as INVALID, not as a count.
                .and_then(|e| e.permission_args().map(|_| e))
            {
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
    match crate::agent_run::headless_unsupported() {
        Some(reason) => outln!("  headless agent handoff: UNSUPPORTED — {reason}"),
        None => outln!("  headless agent handoff: supported"),
    }
}

fn plugin_trust_rows(cfg: &Config, config_path: &std::path::Path) -> Vec<serde_json::Value> {
    let config_dir = config_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let (plugins, _) = thegn_svc::plugin::discover(cfg, &config_dir);
    plugins
        .into_iter()
        .filter(|plugin| plugin.spec.enabled)
        .map(|plugin| {
            let trust = plugin.spec.native_trust().status_token();
            let source = if let Some(dir) = &plugin.dir {
                let mut source = dir.display().to_string();
                if plugin.trusted_native_ignored {
                    source.push_str(" (trusted_native in plugin.toml ignored)");
                }
                source
            } else {
                "[[plugins]]".to_string()
            };
            serde_json::json!({
                "id": plugin.spec.manifest.id.as_str(),
                "source": source,
                "trust": trust,
                "gated_by": plugin.spec.gated_by(),
            })
        })
        .collect()
}

fn plugins_trust_json(cfg: &Config, config_path: &std::path::Path) -> serde_json::Value {
    serde_json::Value::Array(plugin_trust_rows(cfg, config_path))
}

fn plugins_trust_report(cfg: &Config, config_path: &std::path::Path) {
    outln!("Plugins (native code; capabilities are not a sandbox)");
    let rows = plugin_trust_rows(cfg, config_path);
    if rows.is_empty() {
        let config_dir = config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let (all_plugins, _) = thegn_svc::plugin::discover(cfg, &config_dir);
        if all_plugins.is_empty() {
            outln!("  no plugins configured");
        } else {
            outln!("  no enabled plugins ({} configured)", all_plugins.len());
        }
        return;
    }
    for row in rows {
        let gated_by = match row["gated_by"].as_array() {
            Some(values) => values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            None => String::new(),
        };
        let trust = if row["trust"] == "refused-untrusted" {
            "refused-untrusted (needs trusted_native = true)"
        } else {
            row["trust"].as_str().unwrap_or("")
        };
        outln!(
            "  {}  {}  gated by [{}]  {}",
            row["id"].as_str().unwrap_or(""),
            trust,
            gated_by,
            row["source"].as_str().unwrap_or(""),
        );
    }
}

#[cfg(test)]
#[test]
fn plugins_trust_json_reports_trusted_refused_and_ungated() {
    let spec = |id: &str, capability: &str, trusted_native: bool| {
        toml::from_str::<thegn_core::plugin_api::PluginSpec>(&format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"1\"\napi = \"0.3.0\"\ncapabilities = [\"{capability}\"]\ncommand = [\"true\"]\ntrusted_native = {trusted_native}"
        ))
        .unwrap()
    };
    let cfg = Config {
        plugins: vec![
            spec("trusted", "network:example.com", true),
            spec("refused", "network:example.com", false),
            spec("ungated", "surface:statusbar", false),
        ],
        ..Default::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let rows = plugins_trust_json(&cfg, &tmp.path().join("config.toml"));
    assert_eq!(rows[0]["trust"], "trusted-native");
    assert_eq!(rows[1]["trust"], "refused-untrusted");
    assert_eq!(rows[2]["trust"], "ungated");
    assert_eq!(
        rows[1]["gated_by"],
        serde_json::json!(["network:example.com"])
    );
}

/// The effective launch view of every `[[agents]]` entry (harness, model, env
/// overlay keys, permissions) — `doctor --json`'s `agents`.
fn agents_json(cfg: &Config) -> serde_json::Value {
    let agents: Vec<serde_json::Value> = cfg
        .agents
        .iter()
        .map(|a| {
            match thegn_core::agent_task::effective_agent(cfg, &a.name, None)
                .and_then(|e| e.permission_args().map(|_| e))
            {
                Ok(e) => serde_json::json!({
                    "name": a.name,
                    "harness": e.harness,
                    "model": e.model,
                    "env_keys": e.env.keys().cloned().collect::<Vec<_>>(),
                    "permissions": e.permissions,
                }),
                Err(why) => serde_json::json!({ "name": a.name, "error": why }),
            }
        })
        .collect();
    serde_json::Value::Array(agents)
}

fn headless_handoff_json(reason: Option<&str>) -> serde_json::Value {
    serde_json::json!({ "supported": reason.is_none(), "reason": reason })
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
        .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
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

/// The binary a running process is executing, via `/proc/<pid>/exe`.
///
/// No `#[cfg]`: on a platform without procfs the read simply fails and the
/// caller prints "(unknown)", which is the honest answer there.
fn exe_of_pid(pid: i64) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Print the two facts that distinguish "the rebuild took" from "the rebuild
/// took for the CLI only".
///
/// # Why `--version` is not enough
///
/// On 2026-08-29 a v57 daemon drove a v62 database for hours while both the CLI
/// and the daemon reported `0.1.0-alpha.2` — the crate version had not changed,
/// only the schema and the code had. The decisive facts are the **schema pair**
/// (what the database is at vs what this build expects) and the **binary paths**
/// (whether the daemon is executing the same file the CLI is), so `doctor`
/// prints both.
fn build_parity_lines() {
    let build = thegn_core::db::SCHEMA_VERSION;
    match thegn_core::db::on_disk_schema_version() {
        Some(on_disk) if on_disk == build => {
            outln!("  schema        db v{on_disk} == build v{build}");
        }
        Some(on_disk) if on_disk > build => {
            outln!(
                "  schema        db v{on_disk} > build v{build}  ** THIS BUILD IS TOO OLD — \
                 rebuild/reinstall; it cannot see what the newer build writes **"
            );
        }
        Some(on_disk) => {
            outln!("  schema        db v{on_disk} < build v{build} (migrates on next open)");
        }
        None => outln!("  schema        (no database yet) build v{build}"),
    }
    match Db::open_read_only_current() {
        Ok(Some(db)) => match db.space() {
            Ok(space) => {
                let step = thegn_core::plan_maintenance(&space, 1);
                let recommend = space.auto_vacuum != thegn_core::AutoVacuum::Incremental
                    || matches!(step, thegn_core::MaintenanceStep::TooLargeForAuto { .. });
                outln!(
                    "  db space      {} MB ({} MB reclaimable, auto_vacuum {}){}",
                    space.file_bytes().div_ceil(1024 * 1024),
                    space.free_bytes().div_ceil(1024 * 1024),
                    match space.auto_vacuum {
                        thegn_core::AutoVacuum::None => "none",
                        thegn_core::AutoVacuum::Full => "full",
                        thegn_core::AutoVacuum::Incremental => "incremental",
                    },
                    if recommend {
                        "; run 'thegn db vacuum'"
                    } else {
                        ""
                    },
                );
            }
            Err(error) => outln!("  db space      unavailable ({error})"),
        },
        Ok(None) => outln!("  db space      (no database yet)"),
        Err(error) => outln!("  db space      unavailable ({error})"),
    }
    let cli = thegn_core::util::self_exe_path();
    outln!(
        "  cli binary    {}{}",
        cli.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unknown)".into()),
        // A binary rebuilt out from under a live process is the root cause of
        // several unrelated-looking failures at once — self-spawn ENOENT, a
        // refused schema migration, a THEGN_BIN no worker can run — so name the
        // condition here rather than leave each symptom to be diagnosed alone.
        if thegn_core::util::self_exe_is_stale() {
            "  (stale: rebuilt or removed since launch — restart thegn)"
        } else {
            ""
        }
    );
    // Every registered daemon's actual executable — a daemon still running an
    // older build is invisible in its reported version but obvious here.
    use thegn_core::store::ControlStore;
    let Ok(db) = thegn_core::db::Db::open() else {
        return;
    };
    for d in db.daemons().unwrap_or_default() {
        let exe = exe_of_pid(d.pid);
        let same = match (&cli, &exe) {
            (Some(a), Some(b)) => {
                if a == b {
                    "  (same as CLI)"
                } else {
                    "  ** DIFFERENT FROM THE CLI — restart the daemon from this build **"
                }
            }
            _ => "",
        };
        outln!(
            "  daemon binary {}{same}",
            exe.map(|p| p.display().to_string())
                .unwrap_or_else(|| format!("(pid {} — unreadable)", d.pid))
        );
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
    build_parity_lines();
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
/// terminal detection so it is standalone. This compatibility wrapper keeps
/// the existing bundle/tests API for callers that use the default path.
#[cfg(test)]
pub(crate) fn doctor_json(cfg: &Config) -> serde_json::Value {
    let path = thegn_core::config::Config::path();
    let health = super::config_health::collect(&path, None);
    doctor_json_with_health_and_overrides_and_legacy(cfg, &health, &[], Some("absent"))
}

pub(crate) fn doctor_json_with_health(cfg: &Config, health: &ConfigHealth) -> serde_json::Value {
    doctor_json_with_health_and_overrides(cfg, health, &[])
}

fn doctor_json_with_health_and_overrides(
    cfg: &Config,
    health: &ConfigHealth,
    cli_overrides: &[String],
) -> serde_json::Value {
    doctor_json_with_health_and_overrides_and_legacy(cfg, health, cli_overrides, None)
}

fn doctor_json_with_health_and_overrides_and_legacy(
    cfg: &Config,
    health: &ConfigHealth,
    cli_overrides: &[String],
    legacy_state_override: Option<&str>,
) -> serde_json::Value {
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
        "config_health": health.json(),
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
        "compiler_cache": crate::build_cache::sandbox_cache_doctor_json(
            cfg,
            Some(health.main_path.clone()),
            cli_overrides,
        ),
        "toolchain": toolchain_json(cfg),
        "devcontainer": devcontainer_json(cfg),
        "remote_sandbox": remote_sandbox_json(cfg),
        "provider_cache": provider_cache_json(cfg),
        "merge_gate_quarantines": crate::integrate::gate_quarantines().iter().map(|(path, reason)| serde_json::json!({
            "path": path.display().to_string(),
            "reason": reason,
        })).collect::<Vec<_>>(),
        "host_execution": thegn_core::gate::host_execution_json(),
        "managed_tools": managed_tools_json(cfg),
        "mcp_servers": mcp_servers_json(cfg),
        "network": network_json(cfg),
        "providers": providers_json(cfg),
        "sound": crate::notification_sound::SoundRuntime::report(&cfg.notifications.sound),
        "merge_guard": merge_guard_json(cfg),
        "mobile_access": mobile_access_json(cfg),
        "lsp": lsp_json(cfg),
        "system_metrics": system_metrics_json(cfg),
        "monitor_collectors": monitor_collectors_json(),
        "network_path": network_path_json(&thegn_metrics::probe_net_path()),
        "source_control": source_control_json(cfg),
        "harnesses": harness_json(),
        "agents": agents_json(cfg),
        "headless_agent_handoff": headless_handoff_json(
            crate::agent_run::headless_unsupported().as_deref()
        ),
        "skills": super::skills_doctor::inspect(cfg, &super::resolve_worktree(None)),
        "mcp_serve": mcp_serve_scopes_json(cfg),
        "model_proxy": model_proxy_json(cfg),
        "local_control": local_control_security_json_with_legacy_state(cfg, legacy_state_override),
        "remote_control": remote_control_transport_json(cfg),
        "lifecycle_hooks": lifecycle_hooks_json(cfg),
        "worktree_identity": worktree_identity_json(),
        "control_surface_coverage": crate::cmd::api::coverage_json(
            &crate::cmd::api::coverage_report(),
        ),
    })
}

/// Report lifecycle-hook sources without exposing command text. Repo hooks are
/// read only for the diagnostic surface; execution still goes through the
/// normal trust-gated resolver.
fn lifecycle_hooks_json(cfg: &Config) -> serde_json::Value {
    let repo_root = current_repo_root();
    let repo_hooks = repo_root
        .as_deref()
        .and_then(thegn_core::config::load_repo_hooks)
        .map(|(hooks, _)| hooks)
        .unwrap_or_default();
    let db = Db::open().ok();
    let resolved = repo_root
        .as_deref()
        .map(|root| crate::worktree_lifecycle::resolve(cfg, root, db.as_ref()));

    let events = HookEvent::ALL
        .into_iter()
        .map(|event| {
            let global = cfg.hooks.entries(event).len();
            let workspace = cfg
                .workspace
                .values()
                .map(|w| w.hooks.entries(event).len())
                .sum::<usize>();
            let repo = repo_hooks.entries(event).len();
            let trust = if repo == 0 {
                "none"
            } else if db.is_none() {
                "unknown (state DB unavailable)"
            } else if resolved.as_ref().is_some_and(|r| {
                r.pending
                    .iter()
                    .any(|request| request.key == format!("hooks.{}", event.as_str()))
            }) {
                "pending"
            } else {
                "approved"
            };
            (
                event.as_str().to_string(),
                serde_json::json!({
                    "global": global,
                    "workspace": workspace,
                    "repo": repo,
                    "repo_trust": trust,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "repo": repo_root.map(|root| root.display().to_string()),
        "events": events,
    })
}

/// Registry rows quarantined because several worktrees claim one legacy tab
/// name (THE-516). Such a worktree is left exactly where it is, but it is not
/// routed by that tab, so it must be REPORTED — `thegn doctor` is the durable
/// surface for it (the TUI shows a one-line notice at launch).
fn worktree_identity_json() -> serde_json::Value {
    match Db::open().and_then(|db| db.quarantined_worktree_rows()) {
        Ok(rows) => serde_json::json!({
            "state_db": "ok",
            "quarantined": rows
                .into_iter()
                .map(|(worktree, reason)| serde_json::json!({
                    "worktree": worktree,
                    "reason": reason,
                }))
                .collect::<Vec<_>>(),
        }),
        Err(error) => serde_json::json!({
            "state_db": format!("unavailable: {error}"),
            "quarantined": Vec::<serde_json::Value>::new(),
        }),
    }
}

fn worktree_identity_report() {
    let report = worktree_identity_json();
    outln!("Worktree identity");
    let rows = report["quarantined"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(state) = report["state_db"].as_str()
        && state != "ok"
    {
        outln!("  state DB      {state}");
        return;
    }
    if rows.is_empty() {
        outln!("  quarantined   none");
        return;
    }
    outln!("  quarantined   {} worktree(s)", rows.len());
    for row in rows {
        outln!(
            "    {}  ({})",
            row["worktree"].as_str().unwrap_or_default(),
            row["reason"].as_str().unwrap_or_default()
        );
    }
    outln!(
        "  resolve       rename one branch from the sidebar (or `git -C <repo> branch -m`) \
         or remove one checkout with `thegn wt rm`; the tab is re-keyed on the next launch"
    );
}

fn lifecycle_hooks_report(cfg: &Config) {
    let report = lifecycle_hooks_json(cfg);
    outln!("Lifecycle hooks ([hooks])");
    if let Some(repo) = report["repo"].as_str() {
        outln!("  repo          {repo}");
    } else {
        outln!("  repo          (not inside a repository)");
    }
    for event in HookEvent::ALL {
        let row = &report["events"][event.as_str()];
        outln!(
            "  {:<13} global={} workspace={} repo={} trust={}",
            event.as_str(),
            row["global"],
            row["workspace"],
            row["repo"],
            row["repo_trust"].as_str().unwrap_or("unknown"),
        );
    }
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

pub fn run(
    cfg: &Config,
    json: bool,
    config_path: std::path::PathBuf,
    repo_context: Option<std::path::PathBuf>,
    cli_overrides: &[String],
) -> Result<()> {
    let health = super::config_health::collect(&config_path, repo_context.as_deref());
    if json {
        let mut report = doctor_json_with_health_and_overrides(cfg, &health, cli_overrides);
        report["plugins"] = plugins_trust_json(cfg, &config_path);
        report["batteries"] =
            super::batteries_doctor::json(&super::batteries_doctor::inspect_env());
        outln!("{}", serde_json::to_string_pretty(&report)?);
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
    let gate_quarantines = crate::integrate::gate_quarantines();
    if gate_quarantines.is_empty() {
        outln!("Merge gate workspaces: none quarantined");
    } else {
        outln!(
            "Merge gate workspaces: {} quarantined; inspect before manual cleanup:",
            gate_quarantines.len()
        );
        for (path, reason) in gate_quarantines {
            outln!("  {} — {}", path.display(), reason);
        }
    }
    outln!(
        "Config health: {} problem(s), {} warning(s); main {}; profile {}; repo {}; detail: `thegn config validate`",
        health.problems(),
        health.warnings,
        health.main_path.display(),
        health
            .profile_path
            .as_deref()
            .map_or("(none)".to_string(), |path| path.display().to_string()),
        health
            .repo_path
            .as_deref()
            .map_or("(none)".to_string(), |path| path.display().to_string()),
    );
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

    outln!("");
    toolchain_report(cfg);

    mcp_proxy_report(cfg);

    outln!("");
    secrets_report(cfg);

    outln!("");
    hostkey_report();

    outln!("");
    exposure_report(cfg);

    outln!("");
    control_surface_report(cfg);

    outln!("");
    completions_report();

    outln!("");
    harness_report(cfg);

    outln!("");
    plugins_trust_report(cfg, &config_path);

    outln!("");
    super::skills_doctor::print(&super::skills_doctor::inspect(
        cfg,
        &super::resolve_worktree(None),
    ));

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
    super::batteries_doctor::print(&super::batteries_doctor::inspect_env());

    outln!("");
    pane_daemon_report(cfg);

    outln!("");
    mobile_access_report(cfg);

    outln!("");
    model_proxy_report(cfg);

    outln!("");
    worktree_identity_report();

    outln!("");
    lifecycle_hooks_report(cfg);

    outln!("");
    sandbox_report(cfg);
    outln!("");
    compiler_cache_report(cfg, &config_path, cli_overrides);
    outln!("");
    for line in thegn_core::gate::host_execution_lines() {
        outln!("{line}");
    }

    outln!("");
    devcontainer_report(cfg);

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
    monitor_collectors_report();

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
        .map(|d| thegn_core::time_policy::saturating_i64(u128::from(d.as_secs())))
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
    // `util::have` retains the existing core probe semantics: unlike shell
    // `command -v`, `which_path` does not check executable bits and returns no
    // result when PATH is unset. These are fixed bare-name diagnostics, so
    // removing the shell process is safe within that established contract.
    let has = |bin: &str| thegn_core::util::have(bin);
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
    let legacy = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    let legacy_state = crate::daemon::legacy_socket_state();
    let legacy_label = match legacy_state {
        crate::daemon::LegacySocketState::Live => "live",
        crate::daemon::LegacySocketState::Stale => "stale",
        crate::daemon::LegacySocketState::Absent => "absent",
    };
    outln!("  legacy socket {} {legacy_label}", legacy.display());
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

/// The top level of the linked worktree containing the current directory.
/// Distinct from [`current_repo_root`], which deliberately follows the common
/// git directory back to the main checkout.
fn current_worktree_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    thegn_core::util::git_out(&cwd, &["rev-parse", "--show-toplevel"])
        .map(|path| std::path::PathBuf::from(path.trim()))
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
        outln!(
            "  merge drivers {} (refused by merge-queue folds; conflicts defer to a human)",
            drivers.join(", ")
        );
    }
    outln!("  sign commits  {}", yn(mq.sign_commits));
    if mq.sign_commits {
        match signing_ready() {
            Ok(()) => outln!("  signing       ready (a fold can sign non-interactively)"),
            Err(e) => outln!("  signing       NOT ready — {e}"),
        }
    }
    if mq.rerere
        && let Some(root) = current_repo_root()
        && let Some(status) = crate::integrate::rerere_cache_status(&root)
        && status.present
    {
        outln!(
            "  rerere        {} (queue-private cache, {} entries, {} resolved)",
            yn(mq.rerere),
            status.entries,
            status.resolved
        );
    } else {
        outln!("  rerere        {} (queue-private cache)", yn(mq.rerere));
    }
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
    let rerere_cache = if mq.rerere {
        root.as_deref()
            .and_then(crate::integrate::rerere_cache_status)
            .map(|status| {
                serde_json::json!({
                    "path": status.path.display().to_string(),
                    "present": status.present,
                    "entries": status.entries,
                    "resolved": status.resolved,
                })
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
        "rerere_cache": rerere_cache,
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
        stage_worker_access_report(cfg);
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
    let (backend, _) = effective_first_ready(cfg, &report);
    let exposure = git_metadata_for_config(cfg, backend);
    outln!("  git metadata  {}", git_metadata_line(&exposure));
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
    stage_worker_access_report(cfg);
}

fn compiler_cache_report(cfg: &Config, config_path: &std::path::Path, cli_overrides: &[String]) {
    let report = crate::build_cache::sandbox_cache_doctor_json(
        cfg,
        Some(config_path.to_path_buf()),
        cli_overrides,
    );
    let wrapper = &report["wrapper"];
    let candidates = &report["candidate_inputs"];
    let data = &report["data"];
    let transport = &report["transport"];
    let contained = &report["contained"];
    outln!("Sandbox compiler cache");
    outln!(
        "  authority     [sandbox].compiler_cache ({})",
        report["origin"].as_str().unwrap_or("unknown origin")
    );
    outln!(
        "  mode          {}",
        report["mode"].as_str().unwrap_or("off")
    );
    outln!(
        "  host wrapper  {}{}",
        if wrapper["host_available"].as_bool() == Some(true) {
            "available"
        } else {
            "missing"
        },
        wrapper["host_path"]
            .as_str()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default()
    );
    outln!(
        "  inputs        disk={} · devshell={} · process={} (inputs are not authority)",
        yn(candidates["disk_sccache"].as_bool() == Some(true)),
        candidates["devshell_wrapper"].as_str().unwrap_or("(none)"),
        candidates["process_wrapper"].as_str().unwrap_or("(none)"),
    );
    outln!(
        "  host data     {} ({})",
        data["host_path"].as_str().unwrap_or("unresolved"),
        if data["host_exists"].as_bool() == Some(true) {
            if data["host_read_only"].as_bool() == Some(true) {
                "read-only"
            } else {
                "present; writable by permission bits"
            }
        } else {
            "not created"
        }
    );
    outln!(
        "  transport     {} · fallback {}",
        transport["kind"].as_str().unwrap_or("none"),
        transport["fail_soft"].as_str().unwrap_or("plain rustc")
    );
    let grants = contained["grants"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    outln!(
        "  grants        {}",
        if grants.is_empty() { "(none)" } else { &grants }
    );
    if let Some(last) = contained["last_launch_probe"].as_object() {
        outln!(
            "  contained     {}: {} ({})",
            last.get("backend")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            if last.get("active").and_then(|v| v.as_bool()) == Some(true) {
                "reachable + writable"
            } else {
                "fallback"
            },
            last.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("no reason")
        );
    } else {
        outln!("  contained     not yet proven (next sandbox launch records the probe)");
    }
    if let Some(fallback) = report["last_runtime_fallback"].as_str() {
        outln!("  last fallback {fallback}");
    }
    outln!("  backend scope bwrap proven; other backends fall back to plain rustc");
    outln!(
        "  remediation   {}",
        report["remediation"].as_str().unwrap_or("use plain rustc")
    );
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
    file_access_report(cfg);
    home_report(cfg);
    network_rules_report(cfg);
}

/// The `$HOME` confidentiality line (THE-215): whether the host `$HOME` is hidden,
/// read-only-visible, or writable for the launch this host would pick under the
/// configured profile. Read-only is NOT confidential — only `hidden` is.
fn home_report(cfg: &Config) {
    use thegn_core::sandbox_mounts::{HomeView, planned_home_view};
    let report = thegn_core::sandbox_support::support_report(
        &shell_chain(cfg),
        &Placement::Local,
        cfg_oci_runtime(cfg),
    );
    let backend = thegn_core::sandbox_support::first_ready(&report)
        .map(|r| r.backend)
        .unwrap_or(Backend::None);
    let profile = cfg.sandbox.profile;
    let view = planned_home_view(profile, backend);
    let note = match view {
        HomeView::Hidden => "private tmpfs + reviewed read-only allowlist",
        HomeView::ReadOnly => "whole host $HOME readable (integrity only, not confidential)",
        HomeView::Writable => "whole host $HOME readable AND writable",
    };
    outln!("  home          {} — {note}", view.as_str());
    for line in format_xdg_omissions(&current_xdg_omissions(cfg)) {
        outln!("                XDG carve-out omitted: {line}");
    }
    if profile.hides_home() && !cfg.sandbox.enabled {
        outln!(
            "                NOT ENFORCED: [sandbox] enabled = false — profile `{}` is not applied, panes run on the host with the whole $HOME",
            profile.as_str()
        );
    } else if profile.hides_home() && view != HomeView::Hidden {
        outln!(
            "                MISSED: profile `{}` demands a hidden $HOME; this host would refuse to launch it",
            profile.as_str()
        );
    }
    // Sealed tiers drop any `[sandbox] mounts` entry reaching the credential deny
    // list. Say which, by path only.
    if profile.hides_home()
        && let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        for p in thegn_core::sandbox_mounts::sealed_dropped_cfg_mounts(
            &cfg.sandbox.mounts,
            std::path::Path::new(&home),
        ) {
            outln!("                dropped mount {p} (reaches a protected $HOME path)");
        }
    }
}

fn network_rules_report(cfg: &Config) {
    if cfg.sandbox.network_allow.is_empty() && cfg.sandbox.network_block.is_empty() {
        return;
    }
    let report = thegn_core::sandbox_support::support_report(
        &shell_chain(cfg),
        &Placement::Local,
        cfg_oci_runtime(cfg),
    );
    let backend = thegn_core::sandbox_support::first_ready(&report)
        .map(|r| r.backend)
        .unwrap_or(Backend::None);
    if let Some(line) = network_rules_line(&cfg.sandbox, backend) {
        outln!("  network rules {line}");
    }
}

fn network_rules_line(sb: &thegn_core::config::SandboxConfig, backend: Backend) -> Option<String> {
    if sb.network_allow.is_empty() && sb.network_block.is_empty() {
        return None;
    }
    if thegn_core::sandbox::lower_universal_deny(sb.network, &sb.network_block)
        == thegn_core::config::Network::None
        && sb.network != thegn_core::config::Network::None
    {
        return Some("network_block contains `*` — launches are offline (network = none)".into());
    }
    thegn_core::sandbox::hostname_rules_note_for(
        sb.network,
        &sb.network_allow,
        &sb.network_block,
        backend,
        &Placement::Local,
    )
    .map(|note| note.trim_start_matches("sandbox ").to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GitMetadataExposure {
    SharedReadWrite { common_dir: std::path::PathBuf },
    InsideWorktree,
    HostFilesystem,
    NotMounted,
    NotInRepository,
    Unmodelled,
}

fn git_metadata_exposure(
    backend: Backend,
    file_access: FileAccess,
    worktree: Option<&std::path::Path>,
    git_common: Option<&std::path::Path>,
) -> GitMetadataExposure {
    let (Some(worktree), Some(git_common)) = (worktree, git_common) else {
        return GitMetadataExposure::NotInRepository;
    };
    if matches!(backend, Backend::Systemd | Backend::None)
        || matches!(file_access, FileAccess::All | FileAccess::Host)
    {
        return GitMetadataExposure::HostFilesystem;
    }
    if matches!(
        backend,
        Backend::Wsl | Backend::WinAppContainer | Backend::WinJobObject
    ) {
        return GitMetadataExposure::Unmodelled;
    }
    if file_access == FileAccess::None {
        return GitMetadataExposure::NotMounted;
    }
    if !backend.is_oci() && backend != Backend::Bwrap {
        return GitMetadataExposure::Unmodelled;
    }
    if git_common == worktree || git_common.starts_with(worktree) {
        GitMetadataExposure::InsideWorktree
    } else {
        GitMetadataExposure::SharedReadWrite {
            common_dir: git_common.to_path_buf(),
        }
    }
}

fn git_metadata_for_config(cfg: &Config, backend: Backend) -> GitMetadataExposure {
    if !cfg.sandbox.enabled {
        return GitMetadataExposure::HostFilesystem;
    }
    let worktree = current_worktree_root();
    let git_common = worktree.as_deref().map(thegn_core::util::git_common_dir);
    let git_common = git_common.map(|common| std::fs::canonicalize(&common).unwrap_or(common));
    git_metadata_exposure(
        backend,
        cfg.sandbox.file_access,
        worktree.as_deref(),
        git_common.as_deref(),
    )
}

fn git_metadata_line(exposure: &GitMetadataExposure) -> String {
    match exposure {
        GitMetadataExposure::SharedReadWrite { common_dir } => format!(
            "shared git dir {} is mounted read-write into sandboxed panes (hooks and refs writable; .git/config pinned read-only); no fenced commit path yet",
            common_dir.display()
        ),
        GitMetadataExposure::InsideWorktree => {
            "git dir is inside the worktree and shares its read-write mount (hooks writable)"
                .to_string()
        }
        GitMetadataExposure::HostFilesystem => {
            "sandboxed panes see the host filesystem (no mount boundary for git metadata)"
                .to_string()
        }
        GitMetadataExposure::NotMounted => {
            "git metadata is not mounted (file_access = none)".to_string()
        }
        GitMetadataExposure::NotInRepository => "not run inside a git worktree".to_string(),
        GitMetadataExposure::Unmodelled => "not modelled for this backend".to_string(),
    }
}

fn git_metadata_json(exposure: &GitMetadataExposure) -> serde_json::Value {
    let (kind, path) = match exposure {
        GitMetadataExposure::SharedReadWrite { common_dir } => {
            ("shared_read_write", Some(common_dir.display().to_string()))
        }
        GitMetadataExposure::InsideWorktree => ("inside_worktree", None),
        GitMetadataExposure::HostFilesystem => ("host_filesystem", None),
        GitMetadataExposure::NotMounted => ("not_mounted", None),
        GitMetadataExposure::NotInRepository => ("not_in_repository", None),
        GitMetadataExposure::Unmodelled => ("unmodelled", None),
    };
    serde_json::json!({
        "exposure": kind,
        "path": path,
        "detail": git_metadata_line(exposure),
    })
}

/// The isolation floor line: the effective minimum, its source, and whether
/// the launch this host would actually pick meets it — computed with the same
/// pure [`thegn_core::sandbox_floor::decide`] the launch path uses.
fn floor_report(cfg: &Config) {
    use thegn_core::config::IsolationFloor;
    use thegn_core::sandbox_floor::{
        FloorDecision, containment_summary, decide, floor_in_force, miss_policy_in_force,
    };
    let floor = floor_in_force(&cfg.sandbox, false);
    let source = if cfg.sandbox.isolation_floor.is_some() {
        "configured"
    } else {
        "default"
    };
    let report = local_backend_report(cfg);
    let (backend, actual) = effective_first_ready(cfg, &report);
    // The strongest class any usable backend could give, for a concrete remedy.
    let best = report
        .iter()
        .filter(|r| r.state.usable())
        .filter_map(|r| r.isolation)
        .max_by_key(|c| c.rank().unwrap_or(0))
        .unwrap_or(actual);
    let isolation =
        thegn_core::sandbox_floor::isolation_report(&cfg.sandbox, backend, actual, best);
    if floor == IsolationFloor::Off && source == "default" {
        let bypass = if !cfg.sandbox.enabled {
            "sandbox disabled"
        } else {
            "backend = \"none\""
        };
        outln!("  floor         off (default floor not applied: {bypass})");
    } else if floor == IsolationFloor::Off {
        outln!("  floor         off (configured)");
    } else {
        outln!(
            "  floor         {} (on miss: {}; {})",
            floor.as_str(),
            miss_policy_in_force(&cfg.sandbox).as_str(),
            source
        );
        match decide(floor, miss_policy_in_force(&cfg.sandbox), actual, best) {
            FloorDecision::Ok => outln!(
                "                met — this host would launch at `{}`",
                actual
            ),
            FloorDecision::BypassProvider => {
                outln!(
                    "                {} — thegn does not enforce the floor on a provider placement",
                    thegn_core::sandbox_floor::PROVIDER_UNVERIFIED
                )
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
                    m.refusal()
                )
            }
        }
    }
    for line in isolation_lines(&isolation) {
        outln!("{line}");
    }
    outln!(
        "{}",
        containment_line(containment_summary(cfg.sandbox.enabled, actual))
    );
    for line in crate::sandbox_admission::route_lines() {
        outln!("{line}");
    }
}

/// Format requested, resolved, and enforced isolation as three stable report lines.
fn isolation_lines(report: &thegn_core::sandbox_floor::IsolationReport) -> [String; 3] {
    [
        format!(
            "  requested     profile `{}`, network `{}`, floor `{}` ({}, on miss: {}), file access `{}`",
            report.requested.profile.as_str(),
            report.requested.network.as_str(),
            report.requested.floor.as_str(),
            if report.requested.floor_configured {
                "configured"
            } else {
                "default"
            },
            report.requested.on_floor_miss.as_str(),
            thegn_core::sandbox_floor::file_access_str(report.requested.file_access),
        ),
        format!(
            "  resolved      `{}` at `{}`",
            report.resolved.backend.label(),
            report.resolved.class.as_str(),
        ),
        format!(
            "  enforced      floor: {}; containment: {}; file access: {}",
            report.enforced.floor.as_str(),
            report.enforced.containment.as_str(),
            if report.enforced.file_access {
                "enforced"
            } else {
                "NOT ENFORCED"
            },
        ),
    ]
}

/// The local support report for the shell chain (empty when the sandbox is disabled).
fn local_backend_report(cfg: &Config) -> Vec<thegn_core::sandbox_support::BackendSupport> {
    if !cfg.sandbox.enabled {
        return Vec::new();
    }
    thegn_core::sandbox_support::support_report(
        &shell_chain(cfg),
        &Placement::Local,
        cfg_oci_runtime(cfg),
    )
}

/// Resolve the backend and class the local shell would actually enter.
fn effective_first_ready(
    cfg: &Config,
    report: &[thegn_core::sandbox_support::BackendSupport],
) -> (Backend, IsolationClass) {
    if !cfg.sandbox.enabled {
        return (Backend::None, IsolationClass::HostProcess);
    }
    thegn_core::sandbox_support::first_ready(report)
        .map(|ready| {
            (
                ready.backend,
                ready.isolation.unwrap_or(IsolationClass::HostProcess),
            )
        })
        .unwrap_or((Backend::None, IsolationClass::HostProcess))
}

/// The human-readable containment state, kept pure for direct formatter tests.
fn containment_line(containment: thegn_core::sandbox_floor::Containment) -> String {
    use thegn_core::sandbox_floor::Containment;
    match containment {
        Containment::Uncontained => "  containment   NOT CONTAINED — sandbox is enabled but this host would launch at host-process (no runtime in the chain is ready); install/start podman, docker or bwrap (see the backend table above)".to_string(),
        Containment::Disabled => "  containment   none — [sandbox] enabled = false: panes run on the host".to_string(),
        Containment::Contained(class) => format!("  containment   contained at `{}`", class.as_str()),
        Containment::ProviderManaged => "  containment   provider-managed (unverified by thegn)".to_string(),
    }
}

/// Report file-scope enforcement for the backend the local shell would enter.
fn file_access_report(cfg: &Config) {
    let report = local_backend_report(cfg);
    let (backend, _) = effective_first_ready(cfg, &report);
    if let Some(line) = file_access_line(cfg.sandbox.file_access, backend) {
        outln!("{line}");
    }
}

/// Pure explanation for a requested file scope that the selected backend cannot enforce.
fn file_access_line(file_access: FileAccess, backend: Backend) -> Option<String> {
    if thegn_core::sandbox_floor::file_access_enforced(backend, file_access) {
        return None;
    }
    let scope = match file_access {
        FileAccess::Worktree => "worktree",
        FileAccess::WorktreePlusCaches => "worktree_plus_caches",
        FileAccess::Custom => "custom",
        FileAccess::All => "all",
        FileAccess::Host => "host",
        FileAccess::None => "none",
    };
    let note = match backend {
        Backend::None => "the pane runs on the host",
        Backend::Systemd => {
            "the pane sees the host root (ProtectSystem/ProtectHome hardening only)"
        }
        Backend::WinJobObject => {
            "the pane runs as a host process; process scoping does not hide filesystem paths"
        }
        Backend::WinAppContainer => {
            "the requested scope is not translated into an AppContainer filesystem policy"
        }
        _ => "this backend does not enforce the requested filesystem scope",
    };
    Some(format!(
        "  file access   requested `{scope}` — NOT ENFORCED on {}: {note}",
        backend.label()
    ))
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
        if let Some(note) = thegn_core::sandbox_cpucap::detected_scope_skipped_note() {
            outln!("  cpu cap note  {note}");
        }
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
        if matches!(res, Resolution::Managed { .. }) {
            let status = crate::managed_tool::installed_provenance(&tool);
            let provenance = match &status {
                crate::managed_tool::ProvenanceStatus::NotApplicable => {
                    "n/a (release download)".to_owned()
                }
                crate::managed_tool::ProvenanceStatus::NotInstalled => "not installed".to_owned(),
                crate::managed_tool::ProvenanceStatus::Verified => "verified".to_owned(),
                crate::managed_tool::ProvenanceStatus::MissingRecord => {
                    "no install record (the next use reinstalls it through the contained installer)"
                        .to_owned()
                }
                crate::managed_tool::ProvenanceStatus::Mismatch(reason) => format!(
                    "MISMATCH: {reason} (refused until reinstalled: remove {})",
                    tool.managed_dir().display()
                ),
            };
            outln!("             provenance: {provenance}");
        }
    }
}

/// Every tool has a provenance status; `external` and a null reason mean the
/// tool resolves through an override or PATH rather than its managed install.
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
            let (provenance, provenance_reason) = match &res {
                Resolution::Managed { .. } => {
                    let status = crate::managed_tool::installed_provenance(&tool);
                    (status.as_str(), status.reason())
                }
                _ => ("external", None),
            };
            report["provenance"] = serde_json::json!(provenance);
            report["provenance_reason"] = serde_json::json!(provenance_reason);
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
    fn medium_word_maps_each_network_medium() {
        assert_eq!(medium_word(thegn_metrics::NetMedium::Ethernet), "ethernet");
        assert_eq!(medium_word(thegn_metrics::NetMedium::Wifi), "wifi");
        assert_eq!(medium_word(thegn_metrics::NetMedium::Unknown), "generic");
    }

    #[test]
    fn network_path_json_explains_selected_interface_and_fallback() {
        let selected = thegn_metrics::MediumSelection {
            medium: thegn_metrics::NetMedium::Ethernet,
            iface: Some("eth0".into()),
            reason: thegn_metrics::MediumReason::DefaultRoute,
        };
        assert_eq!(
            network_path_json(&selected),
            serde_json::json!({
                "medium": "ethernet",
                "interface": "eth0",
                "reason": "default route",
            })
        );

        let ambiguous = thegn_metrics::MediumSelection {
            medium: thegn_metrics::NetMedium::Unknown,
            iface: None,
            reason: thegn_metrics::MediumReason::AmbiguousDefaultRoutes,
        };
        assert_eq!(
            network_path_json(&ambiguous),
            serde_json::json!({
                "medium": "generic",
                "interface": null,
                "reason": "ambiguous default routes",
            })
        );
    }

    #[test]
    fn run_does_not_panic_on_default_config() {
        let cfg = Config::default();
        assert!(run(&cfg, false, Config::path(), None, &[]).is_ok());
        assert!(run(&cfg, true, Config::path(), None, &[]).is_ok());
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
        assert!(plain.contains("summon-project-1"));
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
    fn doctor_json_reports_quarantined_merge_gate_workspaces() {
        let report = doctor_json(&Config::default());
        assert!(report["merge_gate_quarantines"].is_array());
    }

    #[test]
    fn doctor_json_exposes_local_control_identity_and_hardening() {
        let local = doctor_json(&Config::default())["local_control"].clone();
        assert!(local["endpoint"].is_string());
        assert!(local["legacy_socket"]["path"].is_string());
        assert!(matches!(
            local["legacy_socket"]["state"].as_str(),
            Some("live" | "stale" | "absent")
        ));
        assert!(matches!(
            (local["auth"].as_str(), local["peer_identity"].as_str()),
            (Some("same-euid-or-token"), Some("native-effective-uid"))
                | (Some("local-pipe-or-token"), Some("local-only-named-pipe"))
        ));
        assert!(local["hardening"].is_string());
    }

    #[test]
    fn doctor_and_api_expose_the_same_coverage_document() {
        let doctor = doctor_json(&Config::default());
        let api = crate::cmd::api::coverage_json(&crate::cmd::api::coverage_report());
        assert_eq!(doctor["control_surface_coverage"], api);
        assert!(
            api["revision"]
                .as_str()
                .is_some_and(|revision| !revision.is_empty())
        );
        assert!(api["schema_version"].is_number());
    }

    #[test]
    fn doctor_distinguishes_safe_secure_unsafe_and_incomplete_remote_transport() {
        let safe = remote_control_transport_json(&Config::default());
        assert_eq!(safe["status"], "safe-loopback");
        assert_eq!(safe["http"], "http");
        assert_eq!(safe["websocket"], "ws");
        assert_eq!(safe["grpc"], "grpc");

        let mut secure_cfg = Config::default();
        secure_cfg.serve.topology = thegn_core::config::ServeTopology::TlsTerminated;
        secure_cfg.serve.advertise_host = "control.example.test".into();
        let secure = remote_control_transport_json(&secure_cfg);
        assert_eq!(secure["status"], "tls-terminated");
        assert_eq!(secure["http"], "https");
        assert_eq!(secure["websocket"], "wss");
        assert_eq!(secure["grpc"], "grpcs");

        secure_cfg.serve.advertise_host.clear();
        let incomplete = remote_control_transport_json(&secure_cfg);
        assert_eq!(incomplete["status"], "invalid");
        assert!(
            incomplete["error"]
                .as_str()
                .unwrap()
                .contains("advertise_host")
        );

        let mut unsafe_cfg = Config::default();
        unsafe_cfg.serve.bind = "0.0.0.0:5380".into();
        unsafe_cfg.serve.advertise_host = "host.example".into();
        unsafe_cfg.serve.unsafe_allow_plaintext_non_loopback = true;
        assert_eq!(
            remote_control_transport_json(&unsafe_cfg)["status"],
            "unsafe-plaintext"
        );
    }

    #[test]
    fn doctor_json_exposes_lifecycle_hook_sources() {
        let hooks = doctor_json(&Config::default())["lifecycle_hooks"].clone();
        assert!(hooks["events"].is_object());
        for event in HookEvent::ALL {
            let row = &hooks["events"][event.as_str()];
            assert!(row["global"].is_u64());
            assert!(row["workspace"].is_u64());
            assert!(row["repo"].is_u64());
            assert!(row["repo_trust"].is_string());
        }
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
        assert_eq!(
            sandbox_json_with(&cfg, Backend::None, IsolationClass::HostProcess)["home"]["strategy"],
            "portable"
        );
    }

    #[test]
    fn xdg_omission_formatter_reports_path_and_reason_only() {
        let omissions = [thegn_core::sandbox_mounts::XdgOmissionEntry {
            path: std::path::PathBuf::from("/private/state"),
            reason: thegn_core::sandbox_mounts::XdgOmission::SymlinkedRoot,
        }];
        let lines = format_xdg_omissions(&omissions);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("/private/state"));
        assert!(lines[0].contains("XDG root is a symlink"));
    }

    #[test]
    fn full_access_command_classifier_matches_only_known_tokens() {
        for token in [
            "--dangerously-skip-permissions",
            "--dangerously-bypass-approvals-and-sandbox",
            "--yolo",
            "danger-full-access",
        ] {
            assert!(
                has_full_access_token(&format!("agent '{token}'")),
                "{token}"
            );
        }
        assert!(!has_full_access_token("agent --yolo-mode"));
        assert!(!has_full_access_token(
            "agent prompt-mentions-danger-full-access"
        ));
        assert!(has_full_access_token("codex --sandbox=danger-full-access"));
        assert!(has_full_access_token(
            "codex -c sandbox_mode=\"danger-full-access\""
        ));
    }

    #[test]
    fn pipeline_stage_warning_uses_effective_agent_command_and_containment() {
        let mut cfg = Config {
            agents: vec![
                toml::from_str(
                    r#"name = "unsafe"
command = "claude --dangerously-skip-permissions"
"#,
                )
                .unwrap(),
            ],
            ..Config::default()
        };
        cfg.pipeline.stages = vec![
            toml::from_str(
                r#"name = "coder"
agent = "unsafe"
"#,
            )
            .unwrap(),
        ];

        cfg.sandbox.enabled = false;
        assert_eq!(unsafe_full_access_stages(&cfg, Backend::Bwrap).len(), 1);
        cfg.sandbox.enabled = true;
        assert!(unsafe_full_access_stages(&cfg, Backend::Bwrap).is_empty());
        assert_eq!(unsafe_full_access_stages(&cfg, Backend::None).len(), 1);
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
    fn managed_tools_json_reports_provenance_keys() {
        let tools = managed_tools_json(&Config::default());
        let rows = tools.as_array().expect("managed tools array");
        for tool in rows {
            let provenance = tool
                .get("provenance")
                .and_then(serde_json::Value::as_str)
                .expect("provenance string");
            assert!(
                [
                    "external",
                    "not-applicable",
                    "not-installed",
                    "verified",
                    "missing",
                    "mismatch",
                ]
                .contains(&provenance),
                "unexpected provenance {provenance}"
            );
            let reason = tool
                .get("provenance_reason")
                .expect("provenance_reason key");
            assert!(reason.is_null() || reason.is_string());
        }
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
        let v = sandbox_json_with(
            &Config::default(),
            Backend::None,
            IsolationClass::HostProcess,
        );
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
        assert!(v["rerere_cache"].is_null());
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

    fn hard_link_test_supported() -> bool {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("probe");
        let linked = dir.path().join("probe-link");
        std::fs::write(&source, b"probe").unwrap();
        std::fs::hard_link(&source, &linked).unwrap();
        thegn_core::fsperm::extra_hard_links(&std::fs::symlink_metadata(&source).unwrap()) > 0
    }

    #[test]
    fn secret_hard_link_notes_find_sandbox_env_but_skip_single_link_vpn_files() {
        if !hard_link_test_supported() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let sandbox_env = state.path().join("sandbox-env");
        std::fs::create_dir(&sandbox_env).unwrap();
        let vpn_container = state.path().join("vpn/c1");
        std::fs::create_dir_all(&vpn_container).unwrap();
        let secret = sandbox_env.join("x.env");
        std::fs::write(&secret, b"canary").unwrap();
        std::fs::hard_link(&secret, state.path().join("outside")).unwrap();
        std::fs::write(vpn_container.join("environment"), b"single").unwrap();

        let notes = secret_hard_link_notes(state.path());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("x.env"));
        assert!(notes[0].contains("1 other hard link"));
    }

    #[test]
    fn secret_hard_link_notes_are_empty_without_state_entries() {
        assert!(secret_hard_link_notes(tempfile::tempdir().unwrap().path()).is_empty());
    }

    #[test]
    fn network_rules_line_says_dns_only_and_offline() {
        let blocked = thegn_core::config::SandboxConfig {
            network_block: vec!["*".into()],
            ..Default::default()
        };
        let offline = network_rules_line(&blocked, Backend::Bwrap).unwrap();
        assert!(offline.contains("network_block contains `*`"));
        assert!(offline.contains("offline (network = none)"));

        let allowed = thegn_core::config::SandboxConfig {
            network_allow: vec!["example.com".into()],
            ..Default::default()
        };
        let note = network_rules_line(&allowed, Backend::Bwrap).unwrap();
        assert!(note.contains("DNS-name rules only"));
        assert!(note.contains("not applied by this backend"));
        assert!(
            network_rules_line(
                &thegn_core::config::SandboxConfig::default(),
                Backend::Bwrap
            )
            .is_none()
        );
    }

    #[test]
    fn git_metadata_linked_worktree_on_bwrap_and_oci_is_shared_read_write() {
        let worktree = std::path::Path::new("/w/feat");
        let common = std::path::Path::new("/repo/.git");
        for backend in [Backend::Bwrap, Backend::Podman] {
            let exposure = git_metadata_exposure(
                backend,
                FileAccess::WorktreePlusCaches,
                Some(worktree),
                Some(common),
            );
            assert_eq!(
                exposure,
                GitMetadataExposure::SharedReadWrite {
                    common_dir: common.to_path_buf(),
                }
            );
            let line = git_metadata_line(&exposure);
            assert!(line.contains("read-write"));
            assert!(line.contains("hooks"));
        }
    }

    #[test]
    fn git_metadata_main_checkout_is_inside_worktree() {
        let worktree = std::path::Path::new("/w/main");
        let common = worktree.join(".git");
        assert_eq!(
            git_metadata_exposure(
                Backend::Bwrap,
                FileAccess::WorktreePlusCaches,
                Some(worktree),
                Some(&common),
            ),
            GitMetadataExposure::InsideWorktree
        );
    }

    #[test]
    fn git_metadata_host_backends_and_full_access_report_host_filesystem() {
        let worktree = std::path::Path::new("/w/main");
        let common = worktree.join(".git");
        for (backend, file_access) in [
            (Backend::Systemd, FileAccess::Worktree),
            (Backend::None, FileAccess::Worktree),
            (Backend::Bwrap, FileAccess::All),
            (Backend::Bwrap, FileAccess::Host),
        ] {
            assert_eq!(
                git_metadata_exposure(backend, file_access, Some(worktree), Some(&common),),
                GitMetadataExposure::HostFilesystem
            );
        }
        assert!(
            git_metadata_line(&GitMetadataExposure::HostFilesystem).contains("host filesystem")
        );
    }

    #[test]
    fn git_metadata_none_access_unmodelled_and_no_repo() {
        let worktree = std::path::Path::new("/w/feat");
        let common = std::path::Path::new("/repo/.git");
        assert_eq!(
            git_metadata_exposure(
                Backend::Bwrap,
                FileAccess::None,
                Some(worktree),
                Some(common),
            ),
            GitMetadataExposure::NotMounted
        );
        assert_eq!(
            git_metadata_exposure(
                Backend::Wsl,
                FileAccess::Worktree,
                Some(worktree),
                Some(common),
            ),
            GitMetadataExposure::Unmodelled
        );
        assert_eq!(
            git_metadata_exposure(Backend::Bwrap, FileAccess::Worktree, None, Some(common)),
            GitMetadataExposure::NotInRepository
        );
        assert_eq!(
            git_metadata_exposure(Backend::Bwrap, FileAccess::Worktree, Some(worktree), None),
            GitMetadataExposure::NotInRepository
        );
        let json = git_metadata_json(&GitMetadataExposure::SharedReadWrite {
            common_dir: common.to_path_buf(),
        });
        assert_eq!(json["exposure"], "shared_read_write");
        assert_eq!(json["path"], "/repo/.git");
        assert!(
            json["detail"]
                .as_str()
                .unwrap()
                .contains("hooks and refs writable")
        );
    }

    #[test]
    fn git_metadata_sandbox_json_reports_disabled_host_filesystem() {
        let cfg = Config {
            sandbox: thegn_core::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let value = sandbox_json(&cfg);
        assert_eq!(value["git_metadata"]["exposure"], "host_filesystem");
        assert!(
            value["git_metadata"]["detail"]
                .as_str()
                .unwrap()
                .contains("host filesystem")
        );
    }

    #[test]
    fn containment_line_for_each_case() {
        use thegn_core::sandbox_floor::Containment;
        assert!(containment_line(Containment::Disabled).contains("enabled = false"));
        assert!(containment_line(Containment::Uncontained).contains("NOT CONTAINED"));
        assert_eq!(
            containment_line(Containment::Contained(IsolationClass::SharedKernel)),
            "  containment   contained at `shared-kernel`"
        );
        assert!(containment_line(Containment::ProviderManaged).contains("provider-managed"));
    }

    #[test]
    fn file_access_line_systemd_worktree_not_enforced_and_bwrap_none() {
        let line = file_access_line(FileAccess::Worktree, Backend::Systemd)
            .expect("systemd does not enforce a private worktree root");
        assert!(line.contains("requested `worktree` — NOT ENFORCED on systemd"));
        assert!(line.contains("host root (ProtectSystem/ProtectHome hardening only)"));
        assert_eq!(file_access_line(FileAccess::None, Backend::Bwrap), None);
    }

    #[test]
    fn sandbox_json_reports_containment_and_file_access() {
        use thegn_core::config::{IsolationFloor, OnFloorMiss, SandboxBackend, SandboxConfig};
        let cfg = Config {
            sandbox: SandboxConfig {
                backend: SandboxBackend::None,
                file_access: FileAccess::Worktree,
                isolation_floor: Some(IsolationFloor::Off),
                on_floor_miss: OnFloorMiss::Fail,
                ..SandboxConfig::default()
            },
            ..Config::default()
        };
        let value = sandbox_json_with(&cfg, Backend::None, IsolationClass::HostProcess);
        assert_eq!(value["containment"], "not-contained");
        assert_eq!(value["isolation_floor"], "off");
        assert_eq!(value["isolation_floor_source"], "configured");
        assert!(!value["file_access_enforced"].as_bool().unwrap());
        assert!(value["containment"].is_string());
        assert!(value["file_access_enforced"].is_boolean());

        let contained = sandbox_json_with(&cfg, Backend::Bwrap, IsolationClass::SharedKernel);
        assert_eq!(contained["containment"], "contained:shared-kernel");
        assert!(contained["file_access_enforced"].as_bool().unwrap());

        let defaults = Config::default();
        let value = sandbox_json_with(&defaults, Backend::Bwrap, IsolationClass::SharedKernel);
        assert_eq!(value["isolation_floor"], "shared-kernel");
        assert_eq!(value["isolation_floor_source"], "default");
        assert_eq!(value["on_floor_miss"], "fail");
    }

    #[test]
    fn sandbox_json_reports_network_rules_note() {
        let cfg = Config {
            sandbox: thegn_core::config::SandboxConfig {
                network_allow: vec!["example.com".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let report = sandbox_json_with(&cfg, Backend::Bwrap, IsolationClass::SharedKernel);
        assert!(
            report["network_rules"]
                .as_str()
                .unwrap()
                .contains("DNS-name rules only")
        );

        let default_cfg = Config::default();
        let default_report =
            sandbox_json_with(&default_cfg, Backend::Bwrap, IsolationClass::SharedKernel);
        assert!(default_report["network_rules"].is_null());
    }

    #[test]
    fn isolation_report_json_has_requested_resolved_enforced() {
        let cfg = Config::default();
        let contained =
            sandbox_json_with(&cfg, Backend::Bwrap, IsolationClass::SharedKernel)["isolation"]
                .clone();
        assert_eq!(contained["requested"]["isolation_floor"], "shared-kernel");
        assert_eq!(contained["requested"]["isolation_floor_source"], "default");
        assert_eq!(contained["resolved"]["class"], "shared-kernel");
        assert_eq!(contained["enforced"]["floor"], "met");

        let uncontained =
            sandbox_json_with(&cfg, Backend::None, IsolationClass::HostProcess)["isolation"]
                .clone();
        assert_eq!(uncontained["enforced"]["floor"], "missed-refuse");
        assert_eq!(uncontained["enforced"]["containment"], "not-contained");

        let provider =
            sandbox_json_with(&cfg, Backend::None, IsolationClass::ProviderManaged)["isolation"]
                .clone();
        assert_eq!(
            provider["enforced"]["floor"],
            "provider-managed (unverified)"
        );
    }

    #[test]
    fn isolation_report_lines_name_each_layer() {
        let cfg = Config::default();
        let report = thegn_core::sandbox_floor::isolation_report(
            &cfg.sandbox,
            Backend::Bwrap,
            IsolationClass::SharedKernel,
            IsolationClass::SharedKernel,
        );
        let lines = isolation_lines(&report);
        assert!(lines[0].starts_with("  requested"));
        assert!(lines[0].contains("floor `shared-kernel` (default, on miss: fail)"));
        assert!(lines[1].starts_with("  resolved"));
        assert!(lines[1].contains("`bwrap` at `shared-kernel`"));
        assert!(lines[2].starts_with("  enforced"));
        assert!(lines[2].contains("floor: met; containment: contained:shared-kernel"));

        let uncontained = thegn_core::sandbox_floor::isolation_report(
            &cfg.sandbox,
            Backend::None,
            IsolationClass::HostProcess,
            IsolationClass::HostProcess,
        );
        let uncontained_lines = isolation_lines(&uncontained);
        assert!(uncontained_lines[2].contains("floor: missed-refuse; containment: not-contained"));

        let provider = thegn_core::sandbox_floor::isolation_report(
            &cfg.sandbox,
            Backend::None,
            IsolationClass::ProviderManaged,
            IsolationClass::ProviderManaged,
        );
        let provider_lines = isolation_lines(&provider);
        assert!(provider_lines[2].contains("provider-managed (unverified)"));
    }

    #[test]
    fn iroh_reach_json_reports_the_build() {
        let report = channel_json();
        let expected = if thegn_svc::iroh_reach::BUILT {
            "built"
        } else {
            "not built"
        };
        assert_eq!(report["iroh_reach"], expected);
    }

    #[test]
    fn iroh_reach_json_key_is_present() {
        assert!(channel_json().get("iroh_reach").is_some());
    }

    #[test]
    fn headless_handoff_json_reports_support_and_reason() {
        assert_eq!(
            headless_handoff_json(None),
            serde_json::json!({ "supported": true, "reason": null })
        );
        let unsupported = headless_handoff_json(Some("x (THE-235)"));
        assert!(!unsupported["supported"].as_bool().unwrap());
        assert_eq!(unsupported["reason"], "x (THE-235)");
    }

    #[test]
    fn doctor_json_carries_headless_agent_handoff() {
        let report = doctor_json(&Config::default());
        assert!(
            report["headless_agent_handoff"]["supported"]
                .as_bool()
                .is_some()
        );
    }

    #[test]
    fn admission_routes_json_lists_every_audit_route() {
        let value = sandbox_json_with(
            &Config::default(),
            Backend::None,
            IsolationClass::HostProcess,
        );
        let routes = value["admission_routes"]
            .as_array()
            .expect("admission_routes is an array");
        assert_eq!(routes.len(), 11);
        for route in routes {
            assert!(route["id"].as_str().is_some());
            assert!(route["route"].as_str().is_some());
            assert!(route["status"].as_str().is_some());
            assert!(route["path"].as_str().is_some());
        }
        assert_eq!(
            routes
                .iter()
                .map(|route| (
                    route["id"].as_str().expect("route id is a string"),
                    route["status"].as_str().expect("route status is a string"),
                ))
                .collect::<Vec<_>>(),
            crate::sandbox_admission::LAUNCH_ROUTES
                .iter()
                .map(|route| (route.id, route.status.as_str()))
                .collect::<Vec<_>>()
        );
    }
}
