//! `thegn vps-ssh <name> [--] [cmd…]` — the self-bridge that gives VPS envs a
//! CLI exec prefix (the role `sprite exec -s <id> --` / `daytona ssh <id> --`
//! play for their vendors): panes, chrome git/fs reads, and the persisted
//! worktree location all run through it, so the whole provider machinery works
//! without a vendor CLI. Resolves the instance's IP from the vps registry
//! (falling back to the provider API when the record is missing/stale) and
//! **exec**s a real `ssh` with the managed key + per-instance known_hosts.

use anyhow::{Context, Result, anyhow};
use thegn_core::config::Config;
use thegn_svc::vps::{registry, ssh_shim};

/// Resolve the instance IP: registry fast path, then any configured VPS env's
/// API (which re-persists the record — the stale-IP self-heal).
fn resolve_ip(cfg: &Config, name: &str) -> Result<String> {
    if let Some(rec) = registry::read(name)
        && rec.state == "ready"
        && !rec.ip.is_empty()
    {
        return Ok(rec.ip);
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
        let rt = tokio::runtime::Runtime::new()?;
        if let Ok(ip) = rt.block_on(provider.resolve_ip(name)) {
            return Ok(ip);
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
    use std::os::unix::process::CommandExt;

    let ip = resolve_ip(cfg, name)?;
    let (key, _pubkey) = crate::agent::sprite_ssh_keypair()?;
    let shim = ssh_shim::SshShim {
        name: name.to_string(),
        ip,
        user: thegn_svc::vps::VPS_USER.into(),
        key_path: key,
    };
    let mut argv = shim.base_argv();
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
    let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(err).with_context(|| format!("vps-ssh: exec {}", argv.join(" ")))
}
