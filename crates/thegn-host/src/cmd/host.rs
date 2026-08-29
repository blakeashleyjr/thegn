//! `thegn host <action>` — inspect and drive `[host.<name>]` machines: the
//! once-per-host provisioning lifecycle behind fast remote OCI sandboxes.
//! Headless counterpart of the System ▸ Hosts panel; a provision started here
//! and one started in the TUI arbitrate via the DB heartbeat (the second
//! attaches instead of double-driving).

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::host_machine::HostState;
use thegn_core::store::HostStore;
use thegn_core::{msg, outln};

use crate::agent::{ProvisionState, ProvisionStepView};
use crate::host_flow::{ConsentPolicy, HostOutcome, ensure_host_ready, failure_reason};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Add a host without editing config: `thegn host add user@box[:port]`
    /// or `thegn host add dumbpipe:<ticket> --user me`. Persists to the
    /// state DB (declarative `[host.<name>]` config shadows it) and becomes a
    /// selectable env immediately.
    Add {
        /// `user@host[:port]` (ssh) or `dumbpipe:<ticket>` (iroh).
        target: String,
        /// Host name (default: slug of the hostname).
        #[arg(long)]
        name: Option<String>,
        /// SSH user for an iroh (dumbpipe) target.
        #[arg(long)]
        user: Option<String>,
        /// Runtime-install consent for this host: never | ask | auto.
        #[arg(long, default_value = "ask")]
        install: String,
        /// Base-image override (`name[:tag][@sha256:…]`).
        #[arg(long)]
        image: Option<String>,
    },
    /// Remove a DB-added host definition (config-defined hosts are read-only
    /// here) plus its recorded state + inventory. With live placement tenants
    /// this DRAINS instead (see `drain`); `--force` releases them first.
    Rm {
        name: String,
        /// Release live placement tenants and remove anyway.
        #[arg(long)]
        force: bool,
    },
    /// Park the host out of every placement lane (existing sandboxes run to
    /// completion; `rm` finalizes once the last tenant releases).
    Drain { name: String },
    /// List `[host.*]` hosts with reach, state, runtime, and probe age.
    List {
        /// Emit one JSON array instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Full status for one host: state, runtime, inventory, recent events.
    Status { name: String },
    /// Drive the host to Ready (resumes a failed/partial provision).
    Provision {
        name: String,
        /// Pre-grant runtime-install consent (needed headlessly; on a TTY you
        /// are prompted instead).
        #[arg(long)]
        yes: bool,
    },
    /// Re-probe reach + runtime now (refreshes the probe TTL).
    Probe { name: String },
    /// Forget the host's recorded state + inventory (the on-host image/volumes
    /// are labelled `thegn.managed` and can be pruned there).
    RmCache {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Discover remote-host candidates from your tailnet (`tailscale status`),
    /// credential-free. No credential is ever read or stored — Tailscale SSH /
    /// the target's sshd + the tailnet ACLs authorize at connect time. Promote
    /// one to a saved `[host.<name>]` with `--promote <name-or-fqdn>`.
    Discover {
        /// Emit one JSON array instead of the human table.
        #[arg(long)]
        json: bool,
        /// Include offline peers (default: online only, per config).
        #[arg(long)]
        all: bool,
        /// Promote a discovered candidate (its MagicDNS FQDN or short name) to
        /// a saved host. Explicit, credential-free; writes the state DB only.
        #[arg(long, value_name = "NAME|FQDN")]
        promote: Option<String>,
        /// Name for the promoted host (default: a slug of the node name).
        #[arg(long)]
        name: Option<String>,
        /// Runtime-install consent for the promoted host: never | ask | auto.
        #[arg(long, default_value = "ask")]
        install: String,
    },
}

/// Exit codes: 0 ready/ok, 1 fatal, 2 retryable — scripts can retry on 2.
pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Add {
            target,
            name,
            user,
            install,
            image,
        } => add(
            cfg,
            &target,
            name.as_deref(),
            user.as_deref(),
            &install,
            image.as_deref(),
        ),
        Action::Rm { name, force } => rm(cfg, &name, force),
        Action::Drain { name } => drain(cfg, &name),
        Action::List { json } => list(cfg, json),
        Action::Status { name } => status(cfg, &name),
        Action::Provision { name, yes } => provision(cfg, &name, yes),
        Action::Probe { name } => {
            // A probe is a provision drive whose fast path is disarmed by
            // clearing last_probe first (cheap: Ready hosts re-verify only).
            let binding = binding_for(cfg, &name)?;
            let db = Db::open()?;
            let _ = db.host_touch_probe(&binding.id, 0); // best-effort: probe reset is advisory; provision below reports the real failure
            provision(cfg, &name, false)
        }
        Action::RmCache { name, force } => rm_cache(cfg, &name, force),
        Action::Discover {
            json,
            all,
            promote,
            name,
            install,
        } => discover(
            cfg,
            json,
            all,
            promote.as_deref(),
            name.as_deref(),
            &install,
        ),
    }
}

fn binding_for(cfg: &Config, name: &str) -> Result<thegn_core::host_config::HostBinding> {
    cfg.host_binding(name).ok_or_else(|| {
        anyhow::anyhow!("no usable [host.{name}] in the global config (see `thegn config path`)")
    })
}

fn age(now: i64, t: Option<i64>) -> String {
    match t {
        None => "never".into(),
        Some(t) => {
            let d = now.saturating_sub(t);
            if d < 90 {
                format!("{d}s ago")
            } else if d < 5400 {
                format!("{}m ago", d / 60)
            } else {
                format!("{}h ago", d / 3600)
            }
        }
    }
}

fn state_label(state: &HostState) -> String {
    match state {
        HostState::Ready => "ready".into(),
        HostState::Failed(f) => format!("failed ({})", f.step.as_str()),
        other => other.durable_tag().unwrap_or("provisioning").to_string(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn add(
    cfg: &Config,
    target: &str,
    name: Option<&str>,
    user: Option<&str>,
    install: &str,
    image: Option<&str>,
) -> Result<()> {
    use thegn_core::host_config::{InstallConsent, parse_host_target};
    let (derived, mut hc) = parse_host_target(target, user).map_err(|e| anyhow::anyhow!(e))?;
    let name = name.unwrap_or(&derived);
    if cfg.host.contains_key(name) && !is_db_host(name) {
        anyhow::bail!(
            "[host.{name}] is defined in config.toml — edit it there (config shadows DB hosts)"
        );
    }
    hc.install_runtime =
        InstallConsent::from_str_validated(install).map_err(|e| anyhow::anyhow!(e))?;
    if let Some(img) = image {
        hc.image = img.to_string();
    }
    let db = Db::open()?;
    db.put_host_def(name, &hc, now())?;
    outln!(
        "host {name} added ({}) — it is now a selectable env; provision with \
         `thegn host provision {name}` or just open a worktree on it",
        hc.reach.as_str()
    );
    Ok(())
}

/// Whether `name` came from the DB (merge inserts it into cfg.host, so the
/// config map alone can't distinguish; ask the DB).
fn is_db_host(name: &str) -> bool {
    Db::open()
        .ok()
        .and_then(|db| db.host_defs().ok())
        .is_some_and(|defs| defs.iter().any(|(n, _)| n == name))
}

fn rm(cfg: &Config, name: &str, force: bool) -> Result<()> {
    if !is_db_host(name) {
        if cfg.host.contains_key(name) {
            anyhow::bail!("[host.{name}] is config-defined — remove it from config.toml");
        }
        anyhow::bail!("no DB-added host named {name}");
    }
    let db = Db::open()?;
    let id = thegn_core::host::HostId::named(name);
    // Live placement tenants: never kill someone's running sandboxes on a
    // machine thegn is a guest on — drain (park out of every lane) and
    // finalize once they release. `--force` releases the ledger rows first
    // (the sandboxes themselves are the owner's to stop).
    let tenants = {
        use thegn_core::store::PlacementStore;
        db.tenants_of(&id).unwrap_or_default()
    };
    if !tenants.is_empty() && !force {
        drain(cfg, name)?;
        outln!(
            "host {name} still has {} live placement tenant(s): {}",
            tenants.len(),
            tenants
                .iter()
                .map(|t| t.sandbox.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        outln!("draining instead — re-run `thegn host rm {name}` once they release,");
        outln!("or `--force` to release the ledger rows now (sandboxes keep running).");
        return Ok(());
    }
    if force {
        use thegn_core::store::PlacementStore;
        for t in &tenants {
            let _ = db.tenancy_release(&t.sandbox, now()); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }
    }
    {
        use thegn_core::store::PlacementStore;
        let _ = db.capacity_delete(&id); // best-effort: cache write: the capacity row is bookkeeping; the host_delete below reports the real failure
    }
    db.host_delete(&id)?;
    outln!("host {name} removed (definition + recorded state + inventory)");
    outln!(
        "on-host images/volumes are labelled thegn.managed — reclaim them with `thegn sandbox prune --host {name}` (while the host is still reachable)"
    );
    Ok(())
}

/// Park a host out of every placement lane (durable `draining` state):
/// existing sandboxes run to completion, nothing new lands, provisioning
/// refuses it. Works for config- and DB-defined hosts alike.
fn drain(cfg: &Config, name: &str) -> Result<()> {
    let binding = binding_for(cfg, name)?;
    let db = Db::open()?;
    db.host_checkpoint(
        &binding.id,
        name,
        binding.reach.kind(),
        &thegn_core::host_machine::HostState::Draining,
        None,
        now(),
    )?;
    outln!("host {name} is draining: no new placements; existing sandboxes run out");
    Ok(())
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    if cfg.host.is_empty() {
        if json {
            return super::emit_json(&Vec::<()>::new());
        }
        outln!("no [host.*] hosts defined");
        return Ok(());
    }
    let db = Db::open()?;
    let t = now();
    let mut rows_out = Vec::new();
    for (name, hc) in &cfg.host {
        let row = cfg
            .host_binding(name)
            .and_then(|b| db.host_get(&b.id).ok().flatten());
        let (state, runtime, probed) = match &row {
            Some(r) => (
                state_label(&r.state),
                r.caps
                    .as_ref()
                    .and_then(|c| c.runtime.as_ref())
                    .map(|rt| format!("{} {}", rt.kind.as_str(), rt.version))
                    .unwrap_or_else(|| "-".into()),
                age(t, r.last_probe),
            ),
            None => ("unprovisioned".into(), "-".into(), "never".into()),
        };
        if json {
            #[derive(serde::Serialize)]
            struct HostJson<'a> {
                name: &'a str,
                reach: &'a str,
                state: String,
                runtime: String,
                probed: String,
            }
            rows_out.push(serde_json::json!(HostJson {
                name,
                reach: hc.reach.as_str(),
                state,
                runtime,
                probed,
            }));
            continue;
        }
        outln!(
            "{name:<20} {:<6} {state:<22} {runtime:<16} probed {probed}",
            hc.reach.as_str()
        );
    }
    if json {
        return super::emit_json(&rows_out);
    }
    Ok(())
}

fn status(cfg: &Config, name: &str) -> Result<()> {
    let binding = binding_for(cfg, name)?;
    let db = Db::open()?;
    let t = now();
    outln!("host      {name} ({})", binding.id);
    outln!("reach     {}", binding.reach.kind());
    outln!("image     {}", binding.image);
    match db.host_get(&binding.id)? {
        None => outln!("state     unprovisioned"),
        Some(row) => {
            outln!("state     {}", state_label(&row.state));
            if let HostState::Failed(f) = &row.state {
                outln!("error     {}", failure_reason(f));
            }
            if let Some(caps) = &row.caps {
                outln!(
                    "probed    {} · {} {} · egress {:?}{}",
                    age(t, row.last_probe),
                    caps.os,
                    caps.arch,
                    caps.egress,
                    caps.runtime
                        .as_ref()
                        .map(|r| format!(" · {} {}", r.kind.as_str(), r.version))
                        .unwrap_or_default(),
                );
            }
            if let Some(c) = row.install_consent {
                outln!("consent   {}", if c { "granted" } else { "declined" });
            }
            let inv = db.host_inventory(&binding.id)?;
            for e in &inv {
                outln!(
                    "inventory {} {} {} ({}) verified {}",
                    e.key.kind.as_str(),
                    e.key.digest.short(),
                    e.key.arch,
                    e.ref_name,
                    age(t, e.verified_at.or(Some(e.present_at))),
                );
            }
            for (at, step, detail) in db.host_events_recent(&binding.id, 8)? {
                outln!("event     [{}] {step}: {detail}", age(t, Some(at)));
            }
        }
    }
    Ok(())
}

// Interactive prompt + in-place progress line: a real TTY interaction, the
// sanctioned #[expect] case for the stderr macros.
#[expect(clippy::disallowed_macros)]
fn provision(cfg: &Config, name: &str, mut yes: bool) -> Result<()> {
    let binding = binding_for(cfg, name)?;
    // A TTY may answer the consent question up front; headless needs --yes.
    if !yes
        && binding.consent == thegn_core::host_config::InstallConsent::Ask
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
    {
        // Only ask when a runtime install could actually happen: cheap check
        // is not possible pre-probe, so ask conditionally and lazily would
        // park; instead pre-ask only if the host has never probed a runtime.
        let db = Db::open()?;
        let has_runtime = db
            .host_get(&binding.id)?
            .and_then(|r| r.caps)
            .and_then(|c| c.runtime)
            .is_some();
        if !has_runtime {
            eprint!("If {name} has no container runtime, install podman on it? [y/N] ");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line); // best-effort: a failed/EOF read answers no (the default)
            yes = matches!(line.trim(), "y" | "Y" | "yes");
        }
    }
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut render = move |views: &[ProvisionStepView]| {
        if !is_tty {
            return;
        }
        let line = views
            .iter()
            .map(|v| {
                let glyph = match v.state {
                    ProvisionState::Pending => "·",
                    ProvisionState::Active => "…",
                    ProvisionState::Done => "✓",
                    ProvisionState::Failed => "✗",
                };
                match &v.detail {
                    Some(d) if v.state == ProvisionState::Active => {
                        format!("{glyph} {} ({d})", v.label)
                    }
                    _ => format!("{glyph} {}", v.label),
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        eprint!("\r\x1b[2K{line}");
    };
    let result = ensure_host_ready(
        &binding,
        ConsentPolicy::Headless { assume_yes: yes },
        &mut render,
        None,
        &mut |reach| thegn_svc::host::runner_for(reach),
    );
    if is_tty {
        eprintln!();
    }
    match result {
        Ok(HostOutcome::Ready(spec)) => {
            outln!("{name}: ready — image {}", spec.image);
            Ok(())
        }
        Ok(HostOutcome::NotHostBacked) | Ok(HostOutcome::Deferred) => {
            // Unreachable from the CLI entry (binding is explicit, policy is
            // headless); keep an honest message anyway.
            outln!("{name}: nothing to do");
            Ok(())
        }
        Err(f) => {
            msg::error(&format!("{name}: {}", failure_reason(&f)));
            std::process::exit(if f.retryable {
                super::EXIT_RETRYABLE
            } else {
                super::EXIT_ERROR
            });
        }
    }
}

fn rm_cache(cfg: &Config, name: &str, force: bool) -> Result<()> {
    let binding = binding_for(cfg, name)?;
    if !force {
        anyhow::bail!(
            "this forgets {name}'s recorded provisioning state and inventory; \
             rerun with --force (on-host artifacts keep their thegn.managed labels)"
        );
    }
    let db = Db::open()?;
    db.host_delete(&binding.id)?;
    outln!("{name}: state + inventory forgotten; next use re-provisions");
    outln!(
        "on-host thegn.managed images/volumes remain \u{2014} reclaim them with `thegn sandbox prune --host {name}`"
    );
    Ok(())
}

/// `thegn host discover` — enumerate tailnet peers as remote-host candidates
/// (credential-free) and optionally promote one to a saved `[host.<name>]`.
///
/// This is the catalog `host.discover` (read) verb: it lists. `--promote` is an
/// explicit local write (like `thegn host add`) sharing the subcommand.
fn discover(
    cfg: &Config,
    json: bool,
    all: bool,
    promote: Option<&str>,
    name: Option<&str>,
    install: &str,
) -> Result<()> {
    use thegn_core::seam::{ErrorClass, Kind};
    use thegn_svc::host_discovery::build;

    if cfg.host_discovery.kind.is_reserved() {
        anyhow::bail!(
            "[host_discovery] kind = {:?} is reserved (not implemented in this build)",
            cfg.host_discovery.kind.as_str()
        );
    }
    if !cfg.host_discovery.tailnet.enabled {
        anyhow::bail!("host discovery is disabled ([host_discovery.tailnet] enabled = false)");
    }
    // `--all` overrides the online-only default for this one invocation.
    let mut hd = cfg.host_discovery.clone();
    if all {
        hd.tailnet.online_only = false;
    }
    let Some(provider) = build(&hd) else {
        anyhow::bail!("[host_discovery] kind is reserved (not implemented in this build)");
    };
    // On-demand + off-thread: the seam runs the subprocess on the blocking pool.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let candidates = match rt.block_on(provider.discover()) {
        Ok(c) => c,
        Err(e) => {
            msg::error(&format!("host discover: {}", e.message()));
            // Transient (tailscaled unreachable) is retryable; logged out /
            // missing binary are fatal for this run.
            std::process::exit(match e.class() {
                ErrorClass::Transient => super::EXIT_RETRYABLE,
                _ => super::EXIT_ERROR,
            });
        }
    };

    if let Some(sel) = promote {
        return promote_candidate(cfg, &candidates, sel, name, install);
    }

    if json {
        return super::emit_json(&candidates);
    }
    if candidates.is_empty() {
        outln!(
            "no tailnet host candidates{}",
            if all {
                ""
            } else {
                " (online only — pass --all for offline peers)"
            }
        );
        return Ok(());
    }
    outln!(
        "{:<18} {:<30} {:<8} {:<7} {:<14} {:<14} {}",
        "NAME",
        "MAGICDNS",
        "OS",
        "ONLINE",
        "TAILSCALE-SSH",
        "NODE-ID",
        "TAGS"
    );
    for c in &candidates {
        outln!(
            "{:<18} {:<30} {:<8} {:<7} {:<14} {:<14} {}",
            c.name,
            c.fqdn,
            c.os,
            if c.online { "yes" } else { "no" },
            c.ssh.as_str(),
            if c.node_id.is_empty() {
                "-"
            } else {
                c.node_id.as_str()
            },
            c.tags.join(",")
        );
    }
    outln!("");
    outln!(
        "promote one (credential-free): `thegn host discover --promote <name|fqdn>` — \
         the node id above lets you confirm the machine, not just its MagicDNS name."
    );
    Ok(())
}

/// Promote one discovered candidate to a saved `[host.<name>]`-shaped DB def.
/// Credential-free by construction
/// ([`HostCandidate::to_host_config`](thegn_core::tailnet::HostCandidate::to_host_config));
/// the
/// stable node id is echoed so a MagicDNS-name spoof is visible.
fn promote_candidate(
    cfg: &Config,
    candidates: &[thegn_core::tailnet::HostCandidate],
    sel: &str,
    name_override: Option<&str>,
    install: &str,
) -> Result<()> {
    use thegn_core::host_config::InstallConsent;
    let cand = candidates.iter().find(|c| c.matches(sel)).ok_or_else(|| {
        anyhow::anyhow!(
            "no discovered candidate matching {sel:?} \
             (run `thegn host discover` to list; offline peers need `--all`)"
        )
    })?;
    let (derived, mut hc) = cand.to_host_config();
    hc.install_runtime =
        InstallConsent::from_str_validated(install).map_err(|e| anyhow::anyhow!(e))?;
    let name = name_override.unwrap_or(derived.as_str());
    if cfg.host.contains_key(name) && !is_db_host(name) {
        anyhow::bail!(
            "[host.{name}] is defined in config.toml — edit it there (config shadows DB hosts)"
        );
    }
    let db = Db::open()?;
    db.put_host_def(name, &hc, now())?;
    let node = if cand.node_id.is_empty() {
        "unknown".to_string()
    } else {
        cand.node_id.clone()
    };
    outln!(
        "promoted {} → host {name} (ssh, port 22, node {node}) — no credentials stored; \
         Tailscale SSH / the target's sshd + your tailnet ACLs authorize at connect. \
         Provision with `thegn host provision {name}`, or just open a worktree on it.",
        cand.fqdn
    );
    Ok(())
}

/// Parse-only smoke used by unit tests (the interactive paths need a live DB).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_labels_and_ages_render() {
        assert_eq!(state_label(&HostState::Ready), "ready");
        assert!(
            state_label(&HostState::Failed(thegn_core::host::HostFailure {
                step: thegn_core::host::HostStep::Deliver,
                error: "x".into(),
                retryable: true,
            }))
            .contains("deliver")
        );
        assert_eq!(age(100, None), "never");
        assert_eq!(age(100, Some(60)), "40s ago");
        assert_eq!(age(4000, Some(100)), "65m ago");
        assert_eq!(age(100_000, Some(100)), "27h ago");
    }

    #[test]
    fn missing_host_is_a_config_error() {
        let cfg = Config::default();
        assert!(binding_for(&cfg, "nope").is_err());
    }
}
