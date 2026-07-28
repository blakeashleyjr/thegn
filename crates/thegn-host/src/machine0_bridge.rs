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
use std::time::{Duration, SystemTime};

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

/// Negative-cache TTL: after a failed resolve, CONTROL reads short-circuit for
/// this long instead of re-hitting the provider API — a dead/unresolvable VM
/// otherwise turns every chrome git/fs poll into a failing MCP call (subprocess
/// churn, UI glitch, and self-inflicted API rate-limiting). Interactive
/// attaches (`wake`) always retry so a pane open can still revive a parked VM.
const DOWN_TTL: Duration = Duration::from_secs(30);

fn down_path(name: &str) -> PathBuf {
    cache_dir().join(format!("{name}.down"))
}

/// Whether a down-marker written at `mtime` is still fresh at `now`. Future
/// mtimes (clock skew) count as fresh rather than erroring.
fn down_fresh(mtime: SystemTime, now: SystemTime) -> bool {
    now.duration_since(mtime)
        .map(|d| d < DOWN_TTL)
        .unwrap_or(true)
}

/// The recorded failure reason iff the down-marker is fresh.
fn read_down(name: &str) -> Option<String> {
    let p = down_path(name);
    let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
    if !down_fresh(mtime, SystemTime::now()) {
        return None;
    }
    Some(std::fs::read_to_string(&p).ok()?.trim().to_string())
}

fn note_down(name: &str, reason: &str) {
    let _ = std::fs::create_dir_all(cache_dir());
    // best-effort: the marker is an optimization; a miss just re-resolves.
    let _ = std::fs::write(down_path(name), reason);
}

/// Path of the "mosh doesn't work on this VM's network" marker (sibling of the
/// down-marker). Written when a mosh attach fails to establish, cleared on a
/// successful mosh session or on [`clear`] (suspend/destroy).
fn nomosh_path(name: &str) -> PathBuf {
    cache_dir().join(format!("{name}.nomosh"))
}

/// Whether to skip mosh for this VM. **Sticky** — no TTL, unlike the down-marker:
/// UDP-routability (whether the VM's network delivers mosh's ~60000-61000 UDP back
/// to the client) is a *structural* property of the path, not a transient blip, so
/// once a mosh attach times out we go straight to ssh until the VM is recreated
/// (`clear` wipes the marker). Pure over the marker's existence so the `run` branch
/// stays unit-testable without execing.
fn should_skip_mosh(marker_present: bool) -> bool {
    marker_present
}

fn nomosh_present(name: &str) -> bool {
    nomosh_path(name).exists()
}

fn note_nomosh(name: &str) {
    let _ = std::fs::create_dir_all(cache_dir());
    // best-effort: a miss just re-probes mosh next open (pays the ~18s timeout again).
    let _ = std::fs::write(nomosh_path(name), "mosh session failed to establish");
}

fn clear_nomosh(name: &str) {
    let _ = std::fs::remove_file(nomosh_path(name));
}

/// Drop a machine0 sandbox's cached endpoint (call on suspend/destroy so a parked
/// or gone VM's stale IP is never served to a control read). Also wipes the
/// `nomosh` marker so a recreated VM at the same name re-probes mosh.
pub fn clear(name: &str) {
    let _ = std::fs::remove_file(down_path(name));
    let _ = std::fs::remove_file(nomosh_path(name));
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
    // Control reads back off while a fresh down-marker stands — one `stat` +
    // small read instead of a per-poll MCP call against a VM that just failed.
    if !wake && let Some(reason) = read_down(name) {
        return Err(anyhow!(
            "machine0-ssh: {name} recently unreachable ({reason}); backing off"
        ));
    }
    // Keep the last real provider error so the failure the user sees names the
    // actual cause (quota/limit, not-RUNNING, ssh-auth) instead of a generic
    // "could not resolve" — the true reason (e.g. MACHINE_LIMIT_REACHED) would
    // otherwise be swallowed here after only priming the down-marker.
    let mut last_err: Option<String> = None;
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
        match res {
            Ok((ip, user)) => {
                write_cache(name, &ip, &user);
                let _ = std::fs::remove_file(down_path(name));
                return Ok((ip, user, transport));
            }
            // Both wake and control failures prime the backoff (a failed
            // interactive attach is the same dead VM the next poll would hit).
            Err(e) => {
                let msg = format!("{e:#}");
                note_down(name, &msg);
                last_err = Some(msg);
            }
        }
    }
    match last_err {
        Some(cause) => Err(anyhow!(
            "machine0-ssh: could not bring up VM {name:?}: {cause}"
        )),
        None => Err(anyhow!(
            "machine0-ssh: could not resolve VM {name:?} (no machine0 env with a set \
             MACHINE0_API_KEY); provision it first"
        )),
    }
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
    let mut argv = vec![
        "mosh".to_string(),
        format!("--ssh={ssh_opts}"),
        host.clone(),
    ];
    if !cmd.is_empty() {
        argv.push("--".into());
        argv.extend(cmd.iter().cloned());
    }
    argv
}

/// Build the plain-ssh interactive/exec argv (the non-mosh branch): `ssh [-tt]
/// <opts…> user@ip [-- cmd]`. Interactive panes force PTY allocation (`-tt`);
/// captured control reads stay non-tty. Pure over the shim's argv.
fn ssh_argv(shim: &ssh_shim::SshShim, cmd: &[String], interactive: bool) -> Vec<String> {
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
}

/// Undo terminal state a failed mosh client may have left before we exec ssh into
/// the same pane: show cursor, leave the alternate screen, soft-reset (DECSTR),
/// clear SGR attributes, re-enable autowrap. Best-effort direct writes — no
/// `tput`/`reset` subprocess, and deliberately not a hard `\x1bc` RIS (which
/// clobbers scrollback on many terminals).
fn reset_terminal() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?25h\x1b[?1049l\x1b[!p\x1b[0m\x1b[?7h");
    let _ = out.flush();
}

/// Exec ssh (or mosh) to the named machine0 VM, running `cmd` (empty ⇒ a login
/// shell). Replaces this process on the ssh path (the pane/exec owns the PTY
/// directly). The mosh path is *spawned* rather than exec'd so a failed attach
/// (e.g. the VM's network blocks mosh UDP) falls back to plain ssh in this same
/// live pane instead of leaving a dead pane; a sticky `nomosh` marker then makes
/// the next open skip mosh outright.
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
    // mosh-server are both present AND mosh isn't already known-bad for this VM.
    let try_mosh = interactive
        && transport == RemoteTransport::Mosh
        && !should_skip_mosh(nomosh_present(name))
        && local_mosh_ok()
        && mosh_server_present(&shim);

    if try_mosh {
        let margv = mosh_argv(&shim, cmd);
        // SPAWN (not exec) so we survive a mosh failure and can fall back to ssh
        // in this same live pane. mosh owns our stdio/TTY while it runs; this
        // bridge is an off-loop CLI process, so a blocking child wait is fine.
        #[expect(
            clippy::disallowed_methods,
            reason = "off-loop CLI bridge: block on the mosh child so a UDP-blocked \
                      timeout falls back to ssh instead of killing the pane"
        )]
        let status = std::process::Command::new(&margv[0])
            .args(&margv[1..])
            .status();
        match status {
            // Clean mosh session (incl. a normal logout) → mosh works here; drop
            // any stale marker and close the pane as usual.
            Ok(s) if s.success() => {
                clear_nomosh(name);
                return Ok(());
            }
            // Non-zero exit (the UDP-blocked "Timed out waiting for server" case
            // exits non-zero after ~18s) or a spawn error → remember mosh is bad
            // for this VM, reset the terminal mosh may have dirtied, and fall
            // through to the plain-ssh exec below.
            _ => {
                note_nomosh(name);
                reset_terminal();
            }
        }
    }

    // Plain ssh: chosen up front (non-mosh transport, mosh-less image, or a
    // non-tty control read), or reached by falling back from a failed mosh attach.
    let argv = ssh_argv(&shim, cmd, interactive);
    // CLI bridge process: exec replaces us, ssh owns the PTY/stdio from here.
    let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(err).with_context(|| format!("machine0-ssh: exec {}", argv.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_marker_freshness_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(down_fresh(now, now));
        assert!(down_fresh(now - Duration::from_secs(29), now));
        assert!(!down_fresh(now - Duration::from_secs(31), now));
        // Future mtime (clock skew) is fresh, not an error.
        assert!(down_fresh(now + Duration::from_secs(5), now));
    }

    #[test]
    fn nomosh_marker_is_sticky_selection() {
        // Sticky: presence ⇒ skip mosh, absence ⇒ attempt it. No time input,
        // unlike the down-marker's freshness window.
        assert!(should_skip_mosh(true), "marker present ⇒ skip mosh");
        assert!(!should_skip_mosh(false), "no marker ⇒ attempt mosh");
    }

    #[test]
    fn ssh_argv_forces_tty_and_appends_cmd() {
        let shim = ssh_shim::SshShim {
            name: "m0-dev".into(),
            ip: "203.0.113.9".into(),
            user: "root".into(),
            key_path: "/state/ssh/id".into(),
        };
        // Interactive: -tt injected right after "ssh"; host trails.
        let argv = ssh_argv(&shim, &[], true);
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "-tt");
        assert_eq!(argv.last().map(String::as_str), Some("root@203.0.113.9"));
        // Non-interactive control read: no forced PTY.
        let argv = ssh_argv(&shim, &[], false);
        assert!(!argv.iter().any(|a| a == "-tt"), "no -tt when non-tty: {argv:?}");
        // A command is appended after `--`.
        let argv = ssh_argv(&shim, &["/bin/sh".into(), "-lc".into(), "echo hi".into()], true);
        let dd = argv.iter().position(|a| a == "--").expect("-- present");
        assert_eq!(&argv[dd + 1..], &["/bin/sh", "-lc", "echo hi"]);
    }

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
        assert!(
            !argv[1].contains("root@203.0.113.9"),
            "host is split off the --ssh opts"
        );
        assert_eq!(argv[2], "root@203.0.113.9");
        // With a command, it is appended after `--`.
        let argv = mosh_argv(&shim, &["/bin/sh".into(), "-lc".into(), "echo hi".into()]);
        let dd = argv.iter().position(|a| a == "--").expect("-- present");
        assert_eq!(&argv[dd + 1..], &["/bin/sh", "-lc", "echo hi"]);
    }
}
