//! `thegn vps-ssh <name> [--] [cmd…]` — the self-bridge that gives VPS envs
//! **and fly machines** a CLI exec prefix (the role `sprite exec -s <id> --` /
//! `daytona ssh <id> --` play for their vendors): panes, chrome git/fs reads,
//! and the persisted worktree location all run through it, so the whole
//! provider machinery works without a vendor CLI. Resolves the instance's IP
//! from the shared vps/fly ledger (falling back to the VPS or fly provider API
//! when the record is missing/stale) and **exec**s a real `ssh` as the
//! provider's login user with the managed key + per-instance known_hosts.

use anyhow::{Context, Result, anyhow};
use thegn_core::config::{Config, EnvProviderKind};
use thegn_svc::vps::{registry, ssh_shim};

/// The ssh login user on a fly machine (the fly provider's image runs as root).
const FLY_USER: &str = "root";

/// Where to ssh: the instance IP and the provider kind that owns it (which
/// picks the ssh user and the managed-key namespace).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    ip: String,
    provider: String,
}

/// The ssh login user for a ledger `provider` kind.
fn ssh_user_for(provider: &str) -> &'static str {
    match EnvProviderKind::of(provider) {
        EnvProviderKind::Fly => FLY_USER,
        _ => thegn_svc::vps::VPS_USER,
    }
}

/// Resolve the instance endpoint: ledger fast path, then any configured VPS
/// env's API, then any configured fly env's API (each re-persists the record —
/// the stale-IP self-heal).
fn resolve_ip(cfg: &Config, name: &str) -> Result<Endpoint> {
    registry::checked_name(name).context("vps-ssh: invalid instance name")?;
    let record = registry::read_checked(name)
        .context("vps-ssh: the ledger record is unreadable; refusing to guess its address")?;
    if let Some(rec) = &record
        && rec.state == "ready"
        && !rec.ip.is_empty()
    {
        return Ok(Endpoint {
            ip: rec.ip.clone(),
            provider: rec.provider.clone(),
        });
    }
    // No usable record — ask the API via the first VPS-kind env whose token is
    // set (instance names are globally unique per account, so any env works).
    for envc in cfg.env.values() {
        let pc = &envc.provider;
        if !thegn_core::config::vps_provider_kind(&pc.provider) {
            continue;
        }
        let Some(provider) = crate::provider_factory::vps_provider_for(pc, name) else {
            continue;
        };
        // Current-thread: one network call does not need a worker per core.
        // See `agent::provider_runtime`.
        let rt = crate::agent::provider_runtime()?;
        if let Ok(ip) = rt.block_on(provider.resolve_ip(name)) {
            return Ok(Endpoint {
                ip,
                provider: pc.provider.trim().to_string(),
            });
        }
    }
    // Then fly: `control_command_template` routes fly envs over this bridge.
    for envc in cfg.env.values() {
        let pc = &envc.provider;
        if EnvProviderKind::of(&pc.provider) != EnvProviderKind::Fly {
            continue;
        }
        let Some(provider) = crate::provider_factory::fly_provider_for(pc, name) else {
            continue;
        };
        let rt = crate::agent::provider_runtime()?;
        if let Ok(ip) = rt.block_on(provider.resolve_ip(name)) {
            return Ok(Endpoint {
                ip,
                provider: EnvProviderKind::Fly.as_str().to_string(),
            });
        }
    }
    Err(anyhow!(
        "vps-ssh: no registry record or reachable API for instance {name:?}; \
         provision it first (`thegn env provision`)"
    ))
}

/// Exec ssh to the named instance, running `cmd` (empty ⇒ a login shell).
/// Replaces this process on success (the pane/exec owns the PTY directly).
pub fn run(cfg: &Config, name: &str, cmd: &[String]) -> Result<()> {
    use std::io::IsTerminal;

    let name = registry::checked_name(name).context("vps-ssh: invalid instance name")?;
    let Endpoint {
        ip,
        provider: provider_name,
    } = resolve_ip(cfg, name)?;
    let key = thegn_core::managed_ssh::read_unique_instance(&provider_name, name)?
        .map(|record| record.key_path)
        .filter(|path| path.exists())
        .map(Ok)
        .unwrap_or_else(|| crate::agent::sprite_ssh_keypair().map(|pair| pair.0))?;
    let shim = ssh_shim::SshShim {
        name: name.to_string(),
        ip,
        user: ssh_user_for(&provider_name).into(),
        key_path: key,
    };
    shim.prepare_known_hosts()
        .context("vps-ssh: prepare the per-instance known_hosts pin")?;
    let mut argv = shim.base_argv().context("vps-ssh: build the ssh argv")?;
    // Interactive pane (we own a PTY) ⇒ force allocation; captured control
    // reads stay non-tty.
    if std::io::stdin().is_terminal() {
        argv.insert(1, "-tt".into());
    }
    if !cmd.is_empty() {
        argv.push("--".into());
        argv.extend(cmd.iter().cloned());
    }
    // CLI bridge process: exec replaces us, ssh owns the PTY/stdio from here.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
        Err(err).with_context(|| format!("vps-ssh: exec {}", argv.join(" ")))
    }
    // No exec() on Windows: spawn, wait, and mirror the exit code — ssh still
    // owns the console's stdio for its lifetime.
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .with_context(|| format!("vps-ssh: run {}", argv.join(" ")))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ip_rejects_unsafe_name_before_provider_discovery() {
        let error = resolve_ip(&Config::default(), "../x").unwrap_err();
        assert!(error.to_string().contains("invalid instance name"));
        assert!(format!("{error:#}").contains("invalid provider name"));
    }

    #[test]
    fn resolve_ip_refuses_an_unreadable_ledger_record() {
        let _state = crate::testenv::IsolatedState::new();
        let d = thegn_svc::vps::registry::dir();
        thegn_core::fsperm::ensure_private_dir(&d).unwrap();
        std::fs::write(d.join("tg-corrupt.json"), b"not json").unwrap();

        let error = resolve_ip(&Config::default(), "tg-corrupt").unwrap_err();
        assert!(format!("{error:#}").contains("unreadable"));
    }

    #[test]
    fn resolve_ip_serves_a_ready_fly_record_as_root() {
        let _state = crate::testenv::IsolatedState::new();
        let d = thegn_svc::vps::registry::dir();
        thegn_core::fsperm::ensure_private_dir(&d).unwrap();
        thegn_svc::vps::registry::write(&thegn_svc::vps::registry::VpsRecord {
            name: "tg-flyone".into(),
            provider: "fly".into(),
            state: "ready".into(),
            instance_id: "m-1".into(),
            ip: "203.0.113.7".into(),
            created_at: 0,
            owner: String::new(),
            account: String::new(),
        })
        .unwrap();

        let endpoint = resolve_ip(&Config::default(), "tg-flyone").unwrap();
        assert_eq!(endpoint.ip, "203.0.113.7");
        assert_eq!(endpoint.provider, "fly");
        assert_eq!(ssh_user_for(&endpoint.provider), "root");
        assert_eq!(ssh_user_for("hetzner"), thegn_svc::vps::VPS_USER);
    }
}
