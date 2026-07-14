//! `thegn machine0-ssh <name> [--] [cmd…]` — the self-bridge that gives a
//! machine0 env its CLI exec prefix (the role `vps-ssh`/`sprite-exec` play for
//! their providers): the interactive pane, chrome git/fs reads, and the persisted
//! worktree location all run through it, so the whole provider machinery reaches
//! the VM without a vendor CLI.
//!
//! machine0 is MCP-native (no ledger), so the VM's IP + ssh user are resolved via
//! the provider (`vm_get`). Two modes, keyed on whether we own a PTY:
//! - **interactive pane** (stdin is a tty) → `resolve_endpoint` (WAKES a
//!   suspended VM — resume-on-open) and attaches with `-tt`.
//! - **control read** (non-tty git/fs poll) → `peek_endpoint` (never wakes a
//!   parked VM; errors when suspended so the chrome serves cached state).
//!
//! A small on-disk `(ip,user)` cache keeps control reads off the MCP hot path.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use thegn_core::config::{Config, RemoteTransport};
use thegn_svc::vps::ssh_shim;

/// Cached `(ip, user)` for a machine0 sandbox — avoids a `vm_get` per chrome
/// git-tick. Written on every resolve; cleared on suspend/destroy (see [`clear`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Endpoint {
    ip: String,
    user: String,
}

fn cache_dir() -> PathBuf {
    thegn_core::util::thegn_dir().join("machine0")
}

fn cache_path(name: &str) -> PathBuf {
    cache_dir().join(format!("{name}.json"))
}

fn read_cache(name: &str) -> Option<(String, String)> {
    let ep: Endpoint = serde_json::from_slice(&std::fs::read(cache_path(name)).ok()?).ok()?;
    (!ep.ip.is_empty()).then_some((ep.ip, ep.user))
}

fn write_cache(name: &str, ip: &str, user: &str) {
    let _ = std::fs::create_dir_all(cache_dir());
    if let Ok(js) = serde_json::to_vec(&Endpoint {
        ip: ip.to_string(),
        user: user.to_string(),
    }) {
        // best-effort: the cache is an optimization; a miss re-resolves via MCP.
        let _ = std::fs::write(cache_path(name), js);
    }
}

/// Drop a machine0 sandbox's cached endpoint (call on suspend/destroy so a parked
/// or gone VM's stale IP is never served to a control read).
pub fn clear(name: &str) {
    let _ = std::fs::remove_file(cache_path(name));
}

/// Resolve `(ip, ssh user, pane transport)` for the named sandbox. `wake`
/// (interactive pane) starts a suspended VM; otherwise (control read) peek without
/// waking, using the cache fast-path first. The transport is the owning env's
/// `[env.<name>.provider] transport` (default mosh).
fn resolve(cfg: &Config, name: &str, wake: bool) -> Result<(String, String, RemoteTransport)> {
    let mut transport = RemoteTransport::Mosh;
    // Control reads: trust the cache (cheap; a stale entry just fails the ssh and
    // the chrome falls back to cached glyphs). Interactive attaches always
    // re-resolve so a suspended VM is woken and the cache refreshed. (The cache
    // never drives the pane, so its lack of a transport is fine.)
    if !wake && let Some((ip, user)) = read_cache(name) {
        return Ok((ip, user, transport));
    }
    for envc in cfg.env.values() {
        let pc = &envc.provider;
        if pc.provider.trim() != "machine0" {
            continue;
        }
        transport = pc.transport;
        // Sandbox names are globally unique per account, so any machine0 env with
        // a resolvable key works (mirrors `vps_bridge::resolve_ip`).
        let Some(provider) = crate::provider_factory::machine0_provider_for(pc, name) else {
            continue;
        };
        let rt = tokio::runtime::Runtime::new()?;
        let res = if wake {
            rt.block_on(provider.resolve_endpoint(name))
        } else {
            rt.block_on(provider.peek_endpoint(name))
        };
        if let Ok((ip, user)) = res {
            write_cache(name, &ip, &user);
            return Ok((ip, user, transport));
        }
    }
    Err(anyhow!(
        "machine0-ssh: could not resolve VM {name:?} (no machine0 env with a set \
         MACHINE0_API_KEY, or the VM is not running); provision it first"
    ))
}

/// Whether a local `mosh` client is installed (`mosh --version` succeeds).
// The bridge is its own short-lived process (`thegn machine0-ssh`), never the
// event loop, so a probe subprocess is fine; `.output()` captures stdio so it
// never leaks into the pane.
#[expect(clippy::disallowed_methods)]
fn local_mosh_ok() -> bool {
    std::process::Command::new("mosh")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether the VM has `mosh-server` (a cheap ssh probe over the multiplexed
/// master — the mosh `--ssh` bootstrap reuses the same connection).
// Bridge subprocess (off the event loop); `.output()` captures the probe's stdio.
#[expect(clippy::disallowed_methods)]
fn mosh_server_present(shim: &ssh_shim::SshShim) -> bool {
    let mut argv = shim.base_argv();
    argv.push("--".into());
    argv.push("command -v mosh-server >/dev/null 2>&1".into());
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build the `mosh` argv for an interactive pane: `mosh --ssh="<ssh opts>"
/// user@ip [-- cmd]` — the inner ssh carries the managed key + pinned host key +
/// multiplex options (all but the trailing `user@ip`, which becomes mosh's host).
/// mosh allocates its own PTY (no `-tt`). Pure over the shim's argv.
fn mosh_argv(shim: &ssh_shim::SshShim, cmd: &[String]) -> Vec<String> {
    let base = shim.base_argv();
    // base = ["ssh", <opts…>, "user@ip"]; split the host off the end.
    let (host, opts) = base.split_last().expect("ssh base argv is non-empty");
    let ssh_opts = opts
        .iter()
        .map(|a| thegn_core::util::sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let mut argv = vec!["mosh".to_string(), format!("--ssh={ssh_opts}"), host.clone()];
    if !cmd.is_empty() {
        argv.push("--".into());
        argv.extend(cmd.iter().cloned());
    }
    argv
}

/// Exec ssh (or mosh) to the named machine0 VM, running `cmd` (empty ⇒ a login
/// shell). Replaces this process on success (the pane/exec owns the PTY directly).
pub fn run(cfg: &Config, name: &str, cmd: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let interactive = std::io::stdin().is_terminal();
    let (ip, user, transport) = resolve(cfg, name, interactive)?;
    let (key, _pubkey) = crate::agent::sprite_ssh_keypair()?;
    let shim = ssh_shim::SshShim {
        name: name.to_string(),
        ip,
        user,
        key_path: key,
    };

    // Interactive pane over mosh (default) when the local client + the VM's
    // mosh-server are both present; otherwise fall back to plain ssh so a
    // mosh-less image never breaks the pane. Control/non-tty reads always use ssh.
    let argv = if interactive
        && transport == RemoteTransport::Mosh
        && local_mosh_ok()
        && mosh_server_present(&shim)
    {
        mosh_argv(&shim, cmd)
    } else {
        let mut argv = shim.base_argv();
        if interactive {
            // We own a PTY ⇒ force allocation; captured control reads stay non-tty.
            argv.insert(1, "-tt".into());
        }
        if !cmd.is_empty() {
            argv.push("--".into());
            argv.extend(cmd.iter().cloned());
        }
        argv
    };

    // CLI bridge process: exec replaces us, ssh/mosh owns the PTY/stdio from here.
    let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(err).with_context(|| format!("machine0-ssh: exec {}", argv.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mosh_argv_splits_host_and_carries_ssh_opts() {
        let shim = ssh_shim::SshShim {
            name: "m0-dev".into(),
            ip: "203.0.113.9".into(),
            user: "root".into(),
            key_path: "/state/ssh/id".into(),
        };
        let argv = mosh_argv(&shim, &[]);
        assert_eq!(argv[0], "mosh");
        assert!(argv[1].starts_with("--ssh="));
        assert!(argv[1].contains("ssh"), "inner ssh opts present: {argv:?}");
        assert!(!argv[1].contains("root@203.0.113.9"), "host is split off the --ssh opts");
        assert_eq!(argv[2], "root@203.0.113.9");
        // With a command, it is appended after `--`.
        let argv = mosh_argv(&shim, &["/bin/sh".into(), "-lc".into(), "echo hi".into()]);
        let dd = argv.iter().position(|a| a == "--").expect("-- present");
        assert_eq!(&argv[dd + 1..], &["/bin/sh", "-lc", "echo hi"]);
    }
}
