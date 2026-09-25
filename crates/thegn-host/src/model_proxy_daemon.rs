//! Host-side supervision of the `tgproxy` model-proxy daemon.
//!
//! The proxy runs as its own process (never folded into the host binary — the
//! AI-free shell must not compile-depend on it). This module writes the resolved
//! `[model_proxy]` config to a temp file (SecretRefs travel by reference; keys
//! are resolved inside `tgproxy`), launches `tgproxy` off the UI loop with the
//! listen socket as its single-instance lock, restarts crashes with backoff, and
//! stops it gracefully. All calls here are blocking and MUST run off the event
//! loop (they spawn processes and probe the network).

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use thegn_core::config::Config;
use thegn_core::config_model_proxy::ModelProxyConfig;

/// Directory holding the proxy's runtime files (resolved config + pid state).
pub fn runtime_dir() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn/model_proxy")
}

fn config_path() -> PathBuf {
    runtime_dir().join("config.json")
}

fn pid_path() -> PathBuf {
    runtime_dir().join("tgproxy.pid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProxyPidState {
    pid: u32,
    start_time: Option<u64>,
}

fn safe_proxy_pid(pid: u32) -> bool {
    pid != 0 && pid <= i32::MAX as u32
}

/// Parse the deliberately small, strict pid state format. Unknown, duplicate,
/// missing, empty, zero, and non-round-tripping PID values are all rejected.
fn parse_pid_state(raw: &str) -> Option<ProxyPidState> {
    let mut pid = None;
    let mut start_time = None;
    let mut saw_start_time = false;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("pid=") {
            if pid.is_some() {
                return None;
            }
            let value = value.parse::<u32>().ok()?;
            if !safe_proxy_pid(value) {
                return None;
            }
            pid = Some(value);
        } else if let Some(value) = line.strip_prefix("start_time=") {
            if saw_start_time {
                return None;
            }
            saw_start_time = true;
            start_time = Some(match value {
                "unavailable" => None,
                value => Some(value.parse::<u64>().ok()?),
            });
        } else {
            return None;
        }
    }
    Some(ProxyPidState {
        pid: pid?,
        start_time: if saw_start_time {
            start_time?
        } else {
            return None;
        },
    })
}

fn encode_pid_state(pid: u32, start_time: Option<u64>) -> String {
    let start_time = start_time.map_or_else(|| "unavailable".to_string(), |v| v.to_string());
    format!("pid={pid}\nstart_time={start_time}\n")
}

fn stop_pid_state(raw: &str) -> bool {
    let Some(state) = parse_pid_state(raw) else {
        return false;
    };
    crate::platform::terminate_proxy_pid(state.pid, state.start_time)
}

/// Resolves the `tgproxy` executable: a sibling of the running thegn binary
/// (how nix/cargo install them together), else `tgproxy` on `PATH`.
pub fn tgproxy_exe() -> String {
    if let Some(exe) = thegn_core::util::self_exe_path()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("tgproxy");
        if sibling.exists() {
            return sibling.to_string_lossy().into_owned();
        }
    }
    "tgproxy".to_string()
}

/// Parses the configured listen address (loopback default on parse failure).
pub fn listen_addr(cfg: &ModelProxyConfig) -> SocketAddr {
    cfg.listen
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:8383".parse().unwrap())
}

/// Whether the proxy answers on its listen socket right now (a quick TCP probe).
pub fn probe_up(cfg: &ModelProxyConfig) -> bool {
    TcpStream::connect_timeout(&listen_addr(cfg), Duration::from_millis(200)).is_ok()
}

/// Writes the resolved (keys-by-reference) config to the runtime file, returning
/// its path for the `THEGN_MODEL_PROXY_CONFIG` env var.
fn write_config(cfg: &ModelProxyConfig) -> Result<PathBuf> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = config_path();
    let json = serde_json::to_string_pretty(cfg).context("serialize model-proxy config")?;
    // The file carries SecretRef strings (env:/file:), never resolved values.
    let mut f =
        std::fs::File::create(&path).with_context(|| format!("write {}", path.display()))?;
    f.write_all(json.as_bytes())?;
    Ok(path)
}

/// Launches `tgproxy` detached, wrapped into `thegn.slice` like the other
/// background jobs. The listen socket is the lock, so a duplicate launch exits 0.
/// Returns `true` if a process was spawned (it may still lose the bind race).
pub fn spawn(cfg: &ModelProxyConfig) -> Result<bool> {
    let config_file = write_config(cfg)?;
    let exe = tgproxy_exe();
    // Join the shared resource ceiling (fail-safe: unwrapped if no policy).
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(vec![exe]);
    let (program, rest) = argv.split_first().context("empty tgproxy argv")?;
    let mut cmd = thegn_core::util::detached(program);
    cmd.args(rest);
    cmd.env("THEGN_MODEL_PROXY_CONFIG", &config_file);
    let mut child = cmd.spawn().context("spawn tgproxy")?;
    let start_time = crate::platform::proxy_process_start_time(child.id());
    if crate::platform::proxy_identity_required() && start_time.is_none() {
        // The child is still ours here, so use its owned handle for bounded
        // cleanup rather than publishing an unverified pid state.
        let _ = child.kill();
        return Err(anyhow::anyhow!("cannot establish tgproxy process identity"));
    }
    // Publish identity-bound state only after the child identity was captured.
    if let Err(error) = std::fs::write(pid_path(), encode_pid_state(child.id(), start_time)) {
        // Do not leave an untracked daemon behind if state publication fails.
        let _ = child.kill();
        return Err(error).with_context(|| format!("write {}", pid_path().display()));
    }
    Ok(true)
}

/// Runs `tgproxy` in the foreground (the hidden `thegn proxy serve` verb),
/// writing the resolved config and pointing the daemon at it. Blocks until the
/// child exits; returns its exit code.
// Blocking on the child IS this verb: `thegn proxy serve` is a one-shot CLI
// process that exists to be the proxy's supervisor-visible foreground, never the
// event loop (the UI path is `spawn` + `spawn_supervisor` above).
#[expect(clippy::disallowed_methods)]
pub fn serve_foreground(cfg: &ModelProxyConfig) -> Result<i32> {
    let config_file = write_config(cfg)?;
    let exe = tgproxy_exe();
    let status = std::process::Command::new(&exe)
        .env("THEGN_MODEL_PROXY_CONFIG", &config_file)
        .status()
        .with_context(|| format!("exec {exe}"))?;
    Ok(status.code().unwrap_or(1))
}

/// Ensures the proxy is running: a quick probe, then a spawn + short poll if it
/// is down. Returns `true` when the proxy is up afterwards.
pub fn ensure_running(cfg: &ModelProxyConfig) -> Result<bool> {
    if probe_up(cfg) {
        return Ok(true);
    }
    spawn(cfg)?;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if probe_up(cfg) {
            return Ok(true);
        }
    }
    Ok(probe_up(cfg))
}

/// Stops the running proxy via identity-bound recorded state. Returns `true` if
/// a signal was delivered. Malformed, stale, reused, and vanished state is a
/// normal no-op; the state file is removed in every case.
pub fn stop(_cfg: &ModelProxyConfig) -> Result<bool> {
    let path = pid_path();
    let Ok(pid_str) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let sent = stop_pid_state(&pid_str);
    let _ = std::fs::remove_file(path);
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_state_rejects_malformed_and_extreme_values() {
        for raw in [
            "",
            "pid=\nstart_time=1\n",
            "pid=0\nstart_time=1\n",
            "pid=2147483648\nstart_time=1\n",
            "pid=4294967295\nstart_time=1\n",
            "pid=12\n",
            "pid=12\nstart_time=wat\n",
            "pid=12\nstart_time=1\nextra=x\n",
            "pid=12\npid=13\nstart_time=1\n",
        ] {
            assert_eq!(parse_pid_state(raw), None, "accepted {raw:?}");
        }
    }

    #[test]
    fn malformed_state_never_reaches_termination() {
        for raw in ["", "pid=0\nstart_time=1\n", "pid=not-a-pid\nstart_time=1\n"] {
            assert!(!stop_pid_state(raw));
        }
    }
}

/// The outcome of considering an agent/tool for proxy routing.
pub enum ProxyEnvDecision {
    /// The entry did not opt in (`route_via_proxy` unset) — leave it alone.
    NotRequested,
    /// Opted in but skipped, with a reason to surface — the agent launches
    /// unmodified (a down proxy must never strand it).
    Skipped(String),
    /// Inject these env vars pointing the agent at the proxy with an attribution
    /// key.
    Inject(Vec<(String, String)>),
}

/// Decides the proxy env injection for a named agent/tool launch. Performs a
/// liveness probe when the entry opted in: if the proxy is down, injection is
/// skipped so the agent still launches with its normal direct-provider env.
pub fn agent_proxy_env(cfg: &Config, choice: &str, worktree: &str) -> ProxyEnvDecision {
    let opted_in = cfg
        .agents
        .iter()
        .chain(cfg.tools.iter())
        .find(|nc| nc.name == choice)
        .map(|nc| nc.route_via_proxy)
        .unwrap_or(false);
    if !opted_in {
        return ProxyEnvDecision::NotRequested;
    }
    let mp = &cfg.model_proxy;
    if !mp.enabled {
        return ProxyEnvDecision::Skipped("[model_proxy] disabled".to_string());
    }
    if !probe_up(mp) {
        return ProxyEnvDecision::Skipped(format!("proxy unreachable at {}", mp.listen));
    }
    let key = mint_attribution_key(worktree);
    let base = format!("http://{}", listen_addr(mp));
    ProxyEnvDecision::Inject(vec![
        ("ANTHROPIC_BASE_URL".to_string(), base.clone()),
        ("ANTHROPIC_API_KEY".to_string(), key.clone()),
        ("OPENAI_BASE_URL".to_string(), format!("{base}/v1")),
        ("OPENAI_API_KEY".to_string(), key),
    ])
}

/// Mints a per-worktree virtual attribution key. Random nonce → revocable and
/// unguessable; carries the worktree scope for spend accounting. NOT an upstream
/// secret (leaking it leaks attribution + local spend capacity only).
fn mint_attribution_key(worktree: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    h.write(worktree.as_bytes());
    let nonce = format!("{:016x}", h.finish());
    thegn_core::proxy::attribution::encode(&thegn_core::proxy::attribution::AttributionScope {
        scope: format!("worktree:{worktree}"),
        nonce,
        ..Default::default()
    })
}

/// Spawns the background supervisor thread: while `[model_proxy]` is enabled,
/// keep the proxy up, restarting crashes on a `core::backoff` schedule. A no-op
/// when disabled. Runs entirely off the UI loop (blocking probes + spawns).
pub fn spawn_supervisor(cfg: &Config) {
    if !cfg.model_proxy.enabled {
        return;
    }
    let mp = cfg.model_proxy.clone();
    std::thread::Builder::new()
        .name("model-proxy-sup".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let mut failures: u32 = 0;
            loop {
                match ensure_running(&mp) {
                    Ok(true) => failures = 0,
                    _ => failures = failures.saturating_add(1),
                }
                // Steady re-check cadence when healthy; backoff after failures.
                let wait = if failures == 0 {
                    Duration::from_secs(15)
                } else {
                    thegn_core::backoff::calculate_backoff(
                        thegn_core::backoff::ExhaustionKind::ServerError,
                        failures,
                    )
                    .min(Duration::from_secs(120))
                };
                std::thread::sleep(wait);
            }
        })
        // best-effort: a supervisor that never starts must not stop the
        // compositor from coming up. But a background supervisor that silently
        // fails to exist is exactly the kind of absence nobody notices, so log
        // it before dropping the error.
        .inspect_err(|e| {
            tracing::warn!(
                target: "thegn::proxy",
                "model-proxy supervisor thread could not be spawned: {e}"
            );
            // best-effort: spawn failure already warned by the inspect_err above
        })
        .ok();
}
