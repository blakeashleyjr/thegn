//! `superzej host <action>` — inspect and drive `[host.<name>]` machines: the
//! once-per-host provisioning lifecycle behind fast remote OCI sandboxes.
//! Headless counterpart of the System ▸ Hosts panel; a provision started here
//! and one started in the TUI arbitrate via the DB heartbeat (the second
//! attaches instead of double-driving).

use anyhow::Result;
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::host_machine::HostState;
use superzej_core::store::HostStore;
use superzej_core::{msg, outln};

use crate::agent::{ProvisionState, ProvisionStepView};
use crate::host_flow::{ConsentPolicy, HostOutcome, ensure_host_ready, failure_reason};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Add a host without editing config: `superzej host add user@box[:port]`
    /// or `superzej host add dumbpipe:<ticket> --user me`. Persists to the
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
    /// here) plus its recorded state + inventory.
    Rm { name: String },
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
    /// are labelled `superzej.managed` and can be pruned there).
    RmCache {
        name: String,
        #[arg(long)]
        force: bool,
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
        Action::Rm { name } => rm(cfg, &name),
        Action::List { json } => list(cfg, json),
        Action::Status { name } => status(cfg, &name),
        Action::Provision { name, yes } => provision(cfg, &name, yes),
        Action::Probe { name } => {
            // A probe is a provision drive whose fast path is disarmed by
            // clearing last_probe first (cheap: Ready hosts re-verify only).
            let binding = binding_for(cfg, &name)?;
            let db = Db::open()?;
            let _ = db.host_touch_probe(&binding.id, 0);
            provision(cfg, &name, false)
        }
        Action::RmCache { name, force } => rm_cache(cfg, &name, force),
    }
}

fn binding_for(cfg: &Config, name: &str) -> Result<superzej_core::host_config::HostBinding> {
    cfg.host_binding(name).ok_or_else(|| {
        anyhow::anyhow!("no usable [host.{name}] in the global config (see `superzej config path`)")
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
    use superzej_core::host_config::{InstallConsent, parse_host_target};
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
         `superzej host provision {name}` or just open a worktree on it",
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

fn rm(cfg: &Config, name: &str) -> Result<()> {
    if !is_db_host(name) {
        if cfg.host.contains_key(name) {
            anyhow::bail!("[host.{name}] is config-defined — remove it from config.toml");
        }
        anyhow::bail!("no DB-added host named {name}");
    }
    let db = Db::open()?;
    db.host_delete(&superzej_core::host::HostId::named(name))?;
    outln!("host {name} removed (definition + recorded state + inventory)");
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
        && binding.consent == superzej_core::host_config::InstallConsent::Ask
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
            let _ = std::io::stdin().read_line(&mut line);
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
        &mut |reach| superzej_svc::host::runner_for(reach),
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
             rerun with --force (on-host artifacts keep their superzej.managed labels)"
        );
    }
    let db = Db::open()?;
    db.host_delete(&binding.id)?;
    outln!("{name}: state + inventory forgotten; next use re-provisions");
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
            state_label(&HostState::Failed(superzej_core::host::HostFailure {
                step: superzej_core::host::HostStep::Deliver,
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
