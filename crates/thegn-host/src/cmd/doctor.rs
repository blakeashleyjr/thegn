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

/// The honest boundary class a named backend resolves to at `Local` placement,
/// under the configured OCI runtime (`runsc`/`krun` raise it for OCI backends).
fn isolation_of(backend_name: &str, oci_runtime: Option<&str>) -> Option<IsolationClass> {
    let backend = Backend::parse(backend_name)?;
    Some(Capabilities::from_parts(backend, &Placement::Local, false, oci_runtime).isolation)
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
        "home": home_json(cfg),
    })
}

/// The resolved CPU/memory caps + enforcement mechanism for `--json`.
fn limits_json(cfg: &Config) -> serde_json::Value {
    let limits = &cfg.sandbox.limits;
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    serde_json::json!({
        "cpu_per_pane": limits.cpu,
        "cpu_total": thegn_core::sandbox_cpucap::resolve_cpu_total(
            limits.cpu_total.as_deref().unwrap_or("auto"), ncpu),
        "memory": limits.memory,
        "cpu_enforcement": cpu_cap_key(thegn_core::sandbox_cpucap::detect_cpu_cap()),
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

pub fn run(cfg: &Config, json: bool) -> Result<()> {
    let env = TermEnv::from_env();
    let detected = thegn_core::termcaps::detect(&env);
    let resolved = crate::run::resolve_termcaps(cfg);

    if json {
        let v = serde_json::json!({
            "channel": channel_json(),
            "core_deps": core_deps_json(),
            "env": {
                "TERM": env.term,
                "COLORTERM": env.colorterm,
                "TERM_PROGRAM": env.term_program,
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
            "sandbox": sandbox_json(cfg),
            "remote_sandbox": remote_sandbox_json(cfg),
            "provider_cache": provider_cache_json(cfg),
            "managed_tools": managed_tools_json(cfg),
            "mcp_servers": mcp_servers_json(cfg),
            "network": network_json(cfg),
        });
        outln!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    let show = |k: &str, v: &Option<String>| {
        outln!("  {k:<13} {}", v.as_deref().unwrap_or("(unset)"));
    };
    channel_report();
    outln!("");
    core_deps_report();
    outln!("");
    outln!("Terminal environment");
    show("TERM", &env.term);
    show("COLORTERM", &env.colorterm);
    show("TERM_PROGRAM", &env.term_program);
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

    outln!("");
    pane_daemon_report(cfg);

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
    network_report(cfg);

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
    let db = thegn_core::db::Db::open().ok();
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

/// Whether `gh auth status` reports an authenticated account. Robust to a
/// missing `gh` (returns `false`) — the section only reports, never fails.
// off-loop: doctor is a synchronous CLI verb
#[expect(clippy::disallowed_methods)]
fn gh_authenticated() -> bool {
    std::process::Command::new("gh")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
            "  backend       {} (resolved at spawn from backend_chain; not probed here)",
            cfg.sandbox.backend.as_str()
        );
    }
    if let Some(rt) = cfg_oci_runtime(cfg) {
        outln!("  oci_runtime   {rt}{}", oci_runtime_status(rt));
    }
    let chain = shell_chain(cfg);
    let mut all_weak = true;
    for name in &chain {
        match isolation_of(name, cfg_oci_runtime(cfg)) {
            Some(class) => {
                if !matches!(
                    class,
                    IsolationClass::SharedKernel | IsolationClass::HostProcess
                ) {
                    all_weak = false;
                }
                outln!(
                    "    {:<16} {} \u{2014} {}",
                    name,
                    class,
                    class.escape_note()
                );
            }
            None => outln!("    {:<16} (unknown backend)", name),
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
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let per_pane = limits.cpu.as_deref().filter(|s| !s.trim().is_empty());
    let total = thegn_core::sandbox_cpucap::resolve_cpu_total(
        limits.cpu_total.as_deref().unwrap_or("auto"),
        ncpu,
    );
    if per_pane.is_none() && total.is_none() {
        outln!("  cpu cap       (unset)");
        return;
    }
    let mut parts = Vec::new();
    if let Some(c) = per_pane {
        parts.push(format!("{c} cores/pane"));
    }
    if let Some(q) = &total {
        parts.push(format!("{q} total"));
    }
    let mech = thegn_core::sandbox_cpucap::detect_cpu_cap();
    outln!("  cpu cap       {}  ({})", parts.join(" · "), mech.label());
    if let Some(m) = limits.memory.as_deref().filter(|s| !s.trim().is_empty()) {
        outln!("  mem cap       {m}/pane");
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
            serde_json::json!({
                "name": tool.name,
                "tier": res.tier(),
                "path": res.path(),
                "pinned": tool.version,
                "current": matches!(res, Resolution::Managed { current: true, .. }),
            })
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
        assert_eq!(
            isolation_of("bwrap", None),
            Some(IsolationClass::SharedKernel)
        );
        assert_eq!(
            isolation_of("podman", None),
            Some(IsolationClass::SharedKernel)
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
