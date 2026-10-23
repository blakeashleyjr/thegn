//! Host-side supervision of the `tgproxy` model-proxy daemon.
//!
//! The proxy runs as its own process (never folded into the host binary — the
//! AI-free shell must not compile-depend on it). This module writes the resolved
//! `[model_proxy]` config to a temp file (SecretRefs travel by reference; keys
//! are resolved inside `tgproxy`), launches `tgproxy` off the UI loop with the
//! listen socket as its single-instance lock, restarts crashes with backoff, and
//! stops it gracefully. Process and network operations MUST run off the event
//! loop; publishing desired configuration is a non-blocking mutex update.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use thegn_core::config::Config;
use thegn_core::config_model_proxy::ModelProxyConfig;
use thegn_core::db::Db;
use thegn_core::store::{WorkspaceStore, ZoneStore};

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

fn secret_path() -> PathBuf {
    runtime_dir().join("secret")
}

fn host_secret() -> Result<Vec<u8>> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    secret_for_path(&secret_path())
}

fn secret_for_path(path: &std::path::Path) -> Result<Vec<u8>> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).context("generate model-proxy secret")?;
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let parent = path
        .parent()
        .context("model-proxy secret path has no parent")?;
    let name = path
        .file_name()
        .context("model-proxy secret path has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.tmp-{}-{hex}", std::process::id()));
    thegn_core::fsperm::write_owner_only_new(&temporary, hex.as_bytes())
        .with_context(|| format!("write temporary model-proxy secret {}", temporary.display()))?;
    match std::fs::hard_link(&temporary, path) {
        Ok(()) => {
            std::fs::remove_file(&temporary)
                .with_context(|| format!("remove temporary secret {}", temporary.display()))?;
            Ok(random.to_vec())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&temporary)
                .with_context(|| format!("remove temporary secret {}", temporary.display()))?;
            let mut last_error = None;
            for _ in 0..200 {
                match read_host_secret(path) {
                    Ok(secret) => return Ok(secret),
                    Err(error) => last_error = Some(error),
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(last_error.expect("at least one read attempt"))
        }
        Err(e) => {
            std::fs::remove_file(&temporary)
                .with_context(|| format!("remove temporary secret {}", temporary.display()))?;
            Err(e).with_context(|| format!("create owner-only proxy secret {}", path.display()))
        }
    }
}

/// Attribution credentials expire after seven days. A pane or agent that lives
/// longer than this (possible after daemon warm-reattach) receives HTTP 402 on
/// every request until it is respawned with a fresh credential. These are bearer
/// credentials and can be replayed until expiry.
const ATTRIBUTION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn read_host_secret(path: &std::path::Path) -> Result<Vec<u8>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read proxy secret {}", path.display()))?;
    thegn_core::proxy::attribution::parse_secret_hex(raw.trim())
        .with_context(|| format!("invalid model-proxy secret file {}", path.display()))
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
        // A line without `=`, or with any other key, is malformed: reject the
        // whole state rather than parsing a partial identity.
        let (key, value) = line.split_once('=')?;
        match key {
            "pid" => {
                if pid.is_some() {
                    return None;
                }
                let value = value.parse::<u32>().ok()?;
                if !safe_proxy_pid(value) {
                    return None;
                }
                pid = Some(value);
            }
            "start_time" => {
                if saw_start_time {
                    return None;
                }
                saw_start_time = true;
                start_time = Some(match value {
                    "unavailable" => None,
                    value => Some(value.parse::<u64>().ok()?),
                });
            }
            _ => return None,
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

fn executable_regular_file(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && crate::platform::gate_path::executable_bit(&metadata) != Some(false)
    })
}

fn find_tgproxy_in(
    self_dir: Option<&std::path::Path>,
    path_var: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    // Match `tgproxy_exe`: runtime selection is based on existence, even when
    // the sibling cannot be executed. Report that path instead of a PATH copy.
    if let Some(sibling) = self_dir.map(|dir| dir.join("tgproxy"))
        && sibling.exists()
    {
        return Some(sibling);
    }

    path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("tgproxy"))
        .find(|candidate| executable_regular_file(candidate))
}

pub fn find_tgproxy() -> Option<PathBuf> {
    find_tgproxy_in(
        thegn_core::util::self_exe_path()
            .as_deref()
            .and_then(std::path::Path::parent),
        std::env::var_os("PATH").as_deref(),
    )
}

pub fn tgproxy_status_line(found: Option<&std::path::Path>) -> String {
    found.map_or_else(
        || {
            "not installed (source-build-only in this alpha: cargo install --path crates/thegn-proxy, then put tgproxy beside thegn or on PATH)".to_string()
        },
        |path| {
            if executable_regular_file(path) {
                path.display().to_string()
            } else {
                format!("{} (present but not executable)", path.display())
            }
        },
    )
}

/// Parses the configured listen address (loopback default on parse failure).
pub fn listen_addr(cfg: &ModelProxyConfig) -> SocketAddr {
    cfg.listen
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:8383".parse().unwrap())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeState {
    Down,
    Owned,
    Foreign(String),
    Incompatible(u64),
    LocalError(String),
}

fn verify_probe_response(secret: &[u8], challenge: &str, status: u16, body: &[u8]) -> ProbeState {
    if status != 200 {
        let upgrade = if status == 404 {
            "; this may be an older tgproxy; run `thegn proxy stop` to stop the recorded daemon, then restart"
        } else {
            ""
        };
        return ProbeState::Foreign(format!("identity endpoint returned HTTP {status}{upgrade}"));
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ProbeState::Foreign("invalid identity response".into());
    };
    let Some(proof) = value.get("proof").and_then(serde_json::Value::as_str) else {
        return ProbeState::Foreign("missing identity proof".into());
    };
    if thegn_core::proxy::attribution::verify_identity_proof(secret, challenge, proof) {
        let Some(protocol) = value.get("protocol").and_then(serde_json::Value::as_u64) else {
            return ProbeState::Foreign("missing identity protocol".into());
        };
        if protocol != u64::from(thegn_core::proxy::attribution::IDENTITY_PROTOCOL_VERSION) {
            return ProbeState::Incompatible(protocol);
        }
        ProbeState::Owned
    } else {
        ProbeState::Foreign("identity proof did not match".into())
    }
}

fn probe(cfg: &ModelProxyConfig) -> ProbeState {
    let secret = match host_secret() {
        Ok(secret) => secret,
        Err(error) => return ProbeState::LocalError(error.to_string()),
    };
    probe_with(listen_addr(cfg), &secret)
}

fn probe_with(addr: SocketAddr, secret: &[u8]) -> ProbeState {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) else {
        return ProbeState::Down;
    };
    let mut random = [0u8; 16];
    if let Err(e) = getrandom::fill(&mut random) {
        return ProbeState::Foreign(format!("cannot create identity challenge: {e}"));
    }
    let challenge: String = random.iter().map(|b| format!("{b:02x}")).collect();
    if let Err(e) = stream.set_read_timeout(Some(Duration::from_millis(200))) {
        return ProbeState::Foreign(format!("set identity timeout: {e}"));
    }
    if let Err(e) = stream.set_write_timeout(Some(Duration::from_millis(200))) {
        return ProbeState::Foreign(format!("set identity timeout: {e}"));
    }
    if write!(stream, "GET /thegn/identity?challenge={challenge} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_err() { return ProbeState::Foreign("identity request failed".into()); }
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let mut response = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return ProbeState::Foreign("identity response timed out".into());
        };
        if let Err(e) = stream.set_read_timeout(Some(remaining)) {
            return ProbeState::Foreign(format!("set identity timeout: {e}"));
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                response.extend_from_slice(&chunk[..count]);
                if response.len() > 4096 {
                    return ProbeState::Foreign("identity response too large".into());
                }
            }
            Err(e) => return ProbeState::Foreign(format!("identity response unavailable: {e}")),
        }
    }
    let Some(body_start) = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
    else {
        return ProbeState::Foreign("malformed identity HTTP response".into());
    };
    let header_text = String::from_utf8_lossy(&response[..body_start]);
    let status = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    verify_probe_response(secret, &challenge, status, &response[body_start..])
}

/// Whether the configured listener proves this daemon's authenticated identity.
pub fn probe_up(cfg: &ModelProxyConfig) -> bool {
    matches!(probe(cfg), ProbeState::Owned)
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
/// background jobs. A duplicate bind fails; the supervisor verifies an incumbent
/// with the authenticated identity probe before deciding whether to reuse it.
/// Returns `true` if a process was spawned (it may still lose the bind race).
pub fn spawn(cfg: &ModelProxyConfig) -> Result<bool> {
    ensure_spawn_slot()?;
    let config_file = write_config(cfg)?;
    let secret_file = secret_path();
    let _secret = host_secret()?;
    let exe = tgproxy_exe();
    // Join the shared resource ceiling (fail-safe: unwrapped if no policy).
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(vec![exe]);
    let (program, rest) = argv.split_first().context("empty tgproxy argv")?;
    let mut cmd = thegn_core::util::detached(program);
    cmd.args(rest);
    cmd.env("THEGN_MODEL_PROXY_CONFIG", &config_file);
    cmd.env("THEGN_MODEL_PROXY_SECRET_FILE", &secret_file);
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
    *SPAWNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);
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
    let secret_file = secret_path();
    let _secret = host_secret()?;
    let exe = tgproxy_exe();
    let status = std::process::Command::new(&exe)
        .env("THEGN_MODEL_PROXY_CONFIG", &config_file)
        .env("THEGN_MODEL_PROXY_SECRET_FILE", &secret_file)
        .status()
        .with_context(|| format!("exec {exe}"))?;
    Ok(status.code().unwrap_or(1))
}

/// Ensures the proxy is running: a quick probe, then a spawn + short poll if it
/// is down. Returns `true` when the proxy is up afterwards.
pub fn ensure_running(cfg: &ModelProxyConfig) -> Result<bool> {
    let secret = host_secret()
        .with_context(|| format!("cannot load model-proxy secret {}", secret_path().display()))?;
    ensure_running_with_secret(cfg, &secret)
}

fn ensure_running_with_secret(cfg: &ModelProxyConfig, secret: &[u8]) -> Result<bool> {
    let probe = || probe_with(listen_addr(cfg), secret);
    match probe() {
        ProbeState::Owned => return Ok(true),
        ProbeState::Down => {}
        ProbeState::Foreign(reason) => anyhow::bail!(
            "model-proxy port {} is held by a non-thegn or unauthenticated listener ({reason}); stop it or choose another [model_proxy] listen address",
            cfg.listen
        ),
        ProbeState::Incompatible(version) => anyhow::bail!(
            "model-proxy port {} is held by incompatible tgproxy protocol {version}; upgrade or stop it",
            cfg.listen
        ),
        ProbeState::LocalError(reason) => anyhow::bail!(
            "cannot load model-proxy secret {}: {reason}",
            secret_path().display()
        ),
    }
    spawn(cfg)?;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        match probe() {
            ProbeState::Owned => return Ok(true),
            ProbeState::Down => {}
            ProbeState::Foreign(reason) => anyhow::bail!(
                "model-proxy port {} is held by a non-thegn or unauthenticated listener ({reason}); stop it or choose another [model_proxy] listen address",
                cfg.listen
            ),
            ProbeState::Incompatible(version) => anyhow::bail!(
                "model-proxy port {} is held by incompatible tgproxy protocol {version}; upgrade or stop it",
                cfg.listen
            ),
            ProbeState::LocalError(reason) => anyhow::bail!(
                "cannot load model-proxy secret {}: {reason}",
                secret_path().display()
            ),
        }
    }
    match probe() {
        ProbeState::Owned => Ok(true),
        ProbeState::Down => Ok(false),
        ProbeState::Foreign(reason) => anyhow::bail!(
            "model-proxy port {} is held by a non-thegn or unauthenticated listener ({reason}); stop it or choose another [model_proxy] listen address",
            cfg.listen
        ),
        ProbeState::Incompatible(version) => anyhow::bail!(
            "model-proxy port {} is held by incompatible tgproxy protocol {version}; upgrade or stop it",
            cfg.listen
        ),
        ProbeState::LocalError(reason) => anyhow::bail!(
            "cannot load model-proxy secret {}: {reason}",
            secret_path().display()
        ),
    }
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
    let secret = match host_secret() {
        Ok(secret) => secret,
        Err(error) => {
            return ProxyEnvDecision::Skipped(format!("proxy secret unavailable: {error}"));
        }
    };
    agent_proxy_env_with_secret(cfg, choice, worktree, &secret)
}

fn agent_proxy_env_with_secret(
    cfg: &Config,
    choice: &str,
    worktree: &str,
    secret: &[u8],
) -> ProxyEnvDecision {
    agent_proxy_env_with_secret_scopes(cfg, choice, worktree, secret, attribution_scopes)
}

fn agent_proxy_env_with_secret_scopes(
    cfg: &Config,
    choice: &str,
    worktree: &str,
    secret: &[u8],
    scopes: impl FnOnce(&str) -> (Option<String>, Option<String>),
) -> ProxyEnvDecision {
    let mp = &cfg.model_proxy;
    match probe_with(listen_addr(mp), secret) {
        ProbeState::Owned => {}
        ProbeState::Down => {
            return ProxyEnvDecision::Skipped(format!("proxy unreachable at {}", mp.listen));
        }
        ProbeState::Foreign(reason) => {
            return ProxyEnvDecision::Skipped(format!(
                "untrusted listener at {}: {reason}",
                mp.listen
            ));
        }
        ProbeState::Incompatible(version) => {
            return ProxyEnvDecision::Skipped(format!(
                "incompatible tgproxy protocol {version} at {}",
                mp.listen
            ));
        }
        ProbeState::LocalError(reason) => {
            return ProxyEnvDecision::Skipped(format!("proxy secret unavailable: {reason}"));
        }
    }
    let (workspace, zone) = scopes(worktree);
    let key = mint_attribution_key(
        worktree,
        choice,
        workspace.as_deref(),
        zone.as_deref(),
        secret,
    );
    let base = format!("http://{}", listen_addr(mp));
    ProxyEnvDecision::Inject(vec![
        ("ANTHROPIC_BASE_URL".to_string(), base.clone()),
        ("ANTHROPIC_API_KEY".to_string(), key.clone()),
        ("OPENAI_BASE_URL".to_string(), format!("{base}/v1")),
        ("OPENAI_API_KEY".to_string(), key),
    ])
}

fn attribution_scopes(worktree: &str) -> (Option<String>, Option<String>) {
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(%error, "could not open database for proxy attribution scopes");
            return (None, None);
        }
    };
    let workspace = match db.repo_root_for(worktree) {
        Ok(Some(repo)) => Some(format!("workspace:{repo}")),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "could not resolve workspace proxy attribution scope");
            None
        }
    };
    let zone = match db.zone_of_worktree(worktree) {
        Ok(Some(zone)) => Some(format!("zone:{}", zone.name)),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "could not resolve zone proxy attribution scope");
            None
        }
    };
    (workspace, zone)
}

/// Mints a per-worktree signed attribution key. HMAC authenticates its claims;
/// the nonce is only a discriminator. The key carries the worktree and agent
/// scopes for spend accounting and is not an upstream secret.
fn mint_attribution_key(
    worktree: &str,
    choice: &str,
    workspace: Option<&str>,
    zone: Option<&str>,
    secret: &[u8],
) -> String {
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    thegn_core::proxy::attribution::encode_signed(
        &thegn_core::proxy::attribution::AttributionScope {
            scope: format!("worktree:{worktree}"),
            agent: Some(format!("agent:{choice}")),
            nonce,
            workspace: workspace.map(str::to_owned),
            zone: zone.map(str::to_owned),
            upstream: None,
            exp_ms: None,
        },
        secret,
        now.saturating_add(ATTRIBUTION_TTL_MS),
    )
}

/// The `[model_proxy]` table the supervisor should apply, with a generation
/// bumped on startup and every successful config reload.
struct DesiredCell {
    state: std::sync::Mutex<(u64, Option<ModelProxyConfig>)>,
    changed: std::sync::Condvar,
}

impl DesiredCell {
    const fn new() -> Self {
        Self {
            state: std::sync::Mutex::new((0, None)),
            changed: std::sync::Condvar::new(),
        }
    }

    fn publish(&self, cfg: &ModelProxyConfig) -> u64 {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_add(1);
        state.1 = Some(cfg.clone());
        let generation = state.0;
        self.changed.notify_all();
        generation
    }

    fn snapshot(&self) -> (u64, Option<ModelProxyConfig>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wait up to `timeout` for a generation newer than `seen`.
    fn wait_newer(&self, seen: u64, timeout: Duration) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.0 > seen {
            return true;
        }
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |(generation, _)| *generation <= seen)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 > seen
    }

    /// Wait until a newer desired generation is published, without polling.
    fn wait_newer_forever(&self, seen: u64) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = self
            .changed
            .wait_while(state, |(generation, _)| *generation <= seen)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

static SPAWNED: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);
static DESIRED: DesiredCell = DesiredCell::new();
static SUPERVISOR_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorStep {
    Ensure,
    Restart,
    Stop,
    Idle,
}

fn supervisor_step(
    desired: &ModelProxyConfig,
    applied: Option<&ModelProxyConfig>,
) -> SupervisorStep {
    if !desired.enabled {
        if applied.is_some() {
            SupervisorStep::Stop
        } else {
            SupervisorStep::Idle
        }
    } else if applied.is_some_and(|applied| applied != desired) {
        SupervisorStep::Restart
    } else {
        SupervisorStep::Ensure
    }
}

fn reap_spawned_child() -> bool {
    let mut spawned = SPAWNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let status = match spawned.as_mut() {
        Some(child) => child.try_wait(),
        None => return false,
    };
    match status {
        Ok(Some(_status)) => {
            *spawned = None;
            true
        }
        Ok(None) => false,
        Err(error) => {
            tracing::debug!(target: "thegn::proxy", %error, "could not check model-proxy child status");
            false
        }
    }
}

fn ensure_spawn_slot() -> Result<()> {
    let mut spawned = SPAWNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let status = match spawned.as_mut() {
        Some(child) => child.try_wait(),
        None => return Ok(()),
    };
    match status {
        Ok(Some(_status)) => {
            *spawned = None;
            Ok(())
        }
        Ok(None) => Err(anyhow::anyhow!("previous tgproxy child has not exited")),
        Err(error) => Err(error).context("check prior tgproxy child before spawning"),
    }
}

fn wait_for_proxy_exit(old_cfg: &ModelProxyConfig) -> bool {
    for _ in 0..50 {
        if reap_spawned_child() || matches!(probe(old_cfg), ProbeState::Down) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    reap_spawned_child() || matches!(probe(old_cfg), ProbeState::Down)
}

fn supervisor_wait(step: SupervisorStep, failures: u32) -> Option<Duration> {
    if step == SupervisorStep::Idle {
        None
    } else if failures == 0 {
        Some(Duration::from_secs(15))
    } else {
        Some(
            thegn_core::backoff::calculate_backoff(
                thegn_core::backoff::ExhaustionKind::ServerError,
                failures,
            )
            .min(Duration::from_secs(120)),
        )
    }
}

/// Publishes the desired configuration and starts the background supervisor
/// once. Safe to call again on every successful config reload; the supervisor
/// applies changes off the UI loop and restarts crashes on a backoff schedule.
pub fn spawn_supervisor(cfg: &Config) {
    DESIRED.publish(&cfg.model_proxy);
    if !cfg.model_proxy.enabled
        || SUPERVISOR_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    std::thread::Builder::new()
        .name("thegn-proxy-sup".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let mut applied: Option<ModelProxyConfig> = None;
            let mut failures: u32 = 0;
            loop {
                reap_spawned_child();
                let (seen, desired) = DESIRED.snapshot();
                let desired = desired.unwrap_or_default();
                let step = supervisor_step(&desired, applied.as_ref());
                match step {
                    SupervisorStep::Stop => {
                        match stop(&desired) {
                            Ok(true) => {
                                if let Some(old) = applied.as_ref()
                                    && !wait_for_proxy_exit(old)
                                {
                                    tracing::warn!(target: "thegn::proxy", "model proxy did not exit after being disabled");
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(target: "thegn::proxy", %error, "could not stop the disabled model proxy");
                            }
                        }
                        applied = None;
                        failures = 0;
                    }
                    SupervisorStep::Restart => {
                        let old = applied.as_ref().expect("Restart has an applied config");
                        let stopped = match stop(&desired) {
                            Ok(true) => wait_for_proxy_exit(old),
                            Ok(false) => true,
                            Err(error) => {
                                tracing::warn!(target: "thegn::proxy", %error, "could not stop model proxy before applying reloaded config");
                                true
                            }
                        };
                        if !stopped {
                            failures = failures.saturating_add(1);
                            if failures == 1 {
                                tracing::warn!(target: "thegn::proxy", "model proxy did not exit after config reload");
                            }
                        } else {
                            match ensure_running(&desired) {
                                Ok(true) => failures = 0,
                                Ok(false) => {
                                    failures = failures.saturating_add(1);
                                    if failures == 1 {
                                        tracing::warn!(target: "thegn::proxy", "model proxy did not become ready after config reload");
                                    }
                                }
                                Err(error) => {
                                    failures = failures.saturating_add(1);
                                    if failures == 1 {
                                        tracing::warn!(target: "thegn::proxy", %error, "could not apply reloaded model-proxy config");
                                    }
                                }
                            }
                            applied = Some(desired.clone());
                        }
                    }
                    SupervisorStep::Ensure => {
                        match ensure_running(&desired) {
                            Ok(true) => failures = 0,
                            Ok(false) => {
                                failures = failures.saturating_add(1);
                                if failures == 1 {
                                    tracing::warn!(target: "thegn::proxy", "model proxy did not become ready");
                                }
                            }
                            Err(error) => {
                                failures = failures.saturating_add(1);
                                if failures == 1 {
                                    tracing::warn!(target: "thegn::proxy", %error, "could not ensure model proxy is running");
                                }
                            }
                        }
                        applied = Some(desired.clone());
                    }
                    SupervisorStep::Idle => {}
                }
                if step == SupervisorStep::Stop || step == SupervisorStep::Idle {
                    DESIRED.wait_newer_forever(seen);
                    continue;
                }
                if let Some(wait) = supervisor_wait(step, failures) {
                    DESIRED.wait_newer(seen, wait);
                }
            }
        })
        // best-effort: a supervisor that never starts must not stop the
        // compositor from coming up. But a background supervisor that silently
        // fails to exist is exactly the kind of absence nobody notices, so log
        // it before dropping the error.
        .inspect_err(|e| {
            SUPERVISOR_STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!(
                target: "thegn::proxy",
                "model-proxy supervisor thread could not be spawned: {e}"
            );
            // best-effort: spawn failure already warned by the inspect_err above
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

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

    #[test]
    fn fake_listener_never_passes_identity_probe() {
        let secret = b"fake-listener-test-secret";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 512];
                let count = stream.read(&mut request).unwrap();
                assert!(count > 0);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
            }
        });
        let cfg = ModelProxyConfig {
            enabled: true,
            listen: addr.to_string(),
            ..Default::default()
        };
        assert!(matches!(probe_with(addr, secret), ProbeState::Foreign(_)));
        assert!(
            ensure_running_with_secret(&cfg, secret)
                .unwrap_err()
                .to_string()
                .contains("non-thegn")
        );
        let mut app_cfg = Config {
            model_proxy: cfg,
            ..Default::default()
        };
        app_cfg.agents.push(thegn_core::config::NamedCommand {
            name: "test-agent".into(),
            command: "unused".into(),
            hints: vec![],
            provider: None,
            harness: None,
            model: None,
            env: Default::default(),
            permissions: vec![],
            resume: false,
            route_via_proxy: true,
            drawer_scope: None,
            drawer_cwd: None,
        });
        let mut scopes_looked_up = false;
        assert!(matches!(
            agent_proxy_env_with_secret_scopes(&app_cfg, "test-agent", "/tmp/wt", secret, |_| {
                scopes_looked_up = true;
                (None, None)
            }),
            ProxyEnvDecision::Skipped(_)
        ));
        assert!(!scopes_looked_up);
        server.join().unwrap();
    }

    #[test]
    fn concurrent_supervisors_accept_the_same_authenticated_instance() {
        let secret = b"concurrent-supervisor-test-secret".to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_secret = secret.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 128];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut chunk).unwrap();
                    assert_ne!(count, 0, "identity request ended before its headers");
                    request.extend_from_slice(&chunk[..count]);
                    assert!(request.len() <= 512, "identity request headers too large");
                }
                let request = String::from_utf8_lossy(&request);
                let challenge = request
                    .split("challenge=")
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap();
                let proof =
                    thegn_core::proxy::attribution::identity_proof(&server_secret, challenge);
                let body = format!(
                    r#"{{"protocol":{},"proof":"{proof}"}}"#,
                    thegn_core::proxy::attribution::IDENTITY_PROTOCOL_VERSION
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let cfg = ModelProxyConfig {
            listen: addr.to_string(),
            ..Default::default()
        };
        let left_cfg = cfg.clone();
        let right_cfg = cfg;
        let left_secret = secret.clone();
        let right_secret = secret.clone();
        let left = std::thread::spawn(move || {
            ensure_running_with_secret(&left_cfg, &left_secret).unwrap()
        });
        let right = std::thread::spawn(move || {
            ensure_running_with_secret(&right_cfg, &right_secret).unwrap()
        });
        assert!(left.join().unwrap());
        assert!(right.join().unwrap());
        server.join().unwrap();
    }

    #[test]
    fn protocol_and_proof_are_both_required() {
        let secret = b"secret";
        let challenge = "00aa";
        let proof = thegn_core::proxy::attribution::identity_proof(secret, challenge);
        let wrong_version = format!(r#"{{"protocol":2,"proof":"{proof}"}}"#);
        assert_eq!(
            verify_probe_response(secret, challenge, 200, wrong_version.as_bytes()),
            ProbeState::Incompatible(2)
        );
        let wrong_proof = br#"{"protocol":1,"proof":"00"}"#;
        assert!(matches!(
            verify_probe_response(secret, challenge, 200, wrong_proof),
            ProbeState::Foreign(_)
        ));
        let valid = format!(r#"{{"protocol":1,"proof":"{proof}"}}"#);
        assert_eq!(
            verify_probe_response(secret, challenge, 200, valid.as_bytes()),
            ProbeState::Owned
        );
    }

    #[test]
    fn minted_attribution_binds_worktree_and_agent_for_seven_days() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let key = mint_attribution_key(
            "/repo/wt",
            "reviewer",
            Some("workspace:/repo"),
            Some("zone:c"),
            b"mint-test-secret",
        );
        let claims =
            thegn_core::proxy::attribution::verify(&key, b"mint-test-secret", now).unwrap();
        assert_eq!(claims.scope, "worktree:/repo/wt");
        assert_eq!(claims.agent.as_deref(), Some("agent:reviewer"));
        assert_eq!(claims.workspace.as_deref(), Some("workspace:/repo"));
        assert_eq!(claims.zone.as_deref(), Some("zone:c"));
        let remaining = claims.exp_ms.unwrap() - now;
        assert!((ATTRIBUTION_TTL_MS..=ATTRIBUTION_TTL_MS + 1000).contains(&remaining));
    }

    #[test]
    fn old_proxy_404_explains_pid_bound_replacement() {
        let state = verify_probe_response(b"secret", "challenge", 404, b"not found");
        let ProbeState::Foreign(reason) = state else {
            panic!("expected a foreign listener");
        };
        assert!(reason.contains("older tgproxy"));
        assert!(reason.contains("thegn proxy stop"));
        assert!(reason.contains("restart"));
    }

    #[test]
    fn concurrent_secret_creations_read_the_single_winner() {
        let root = std::env::temp_dir().join(format!(
            "tgproxy-secret-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("secret");
        let a = path.clone();
        let b = path.clone();
        let first = std::thread::spawn(move || secret_for_path(&a).unwrap());
        let second = std::thread::spawn(move || secret_for_path(&b).unwrap());
        assert_eq!(first.join().unwrap(), second.join().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn make_tgproxy(directory: &std::path::Path, mode: u32) -> PathBuf {
        let path = directory.join("tgproxy");
        std::fs::write(&path, b"fixture").unwrap();
        if crate::platform::sound_process::test_support::UNIX {
            thegn_core::fsperm::set_mode_for_test(&path, mode).unwrap();
        }
        path
    }

    #[test]
    fn find_tgproxy_prefers_the_sibling_of_thegn() {
        let sibling_dir = tempfile::tempdir().unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        let sibling = make_tgproxy(sibling_dir.path(), 0o755);
        make_tgproxy(path_dir.path(), 0o755);
        let path_var = std::env::join_paths([path_dir.path()]).unwrap();

        assert_eq!(
            find_tgproxy_in(Some(sibling_dir.path()), Some(&path_var)),
            Some(sibling)
        );
    }

    #[test]
    fn find_tgproxy_falls_back_to_path() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let expected = make_tgproxy(second_dir.path(), 0o755);
        let path_var = std::env::join_paths([first_dir.path(), second_dir.path()]).unwrap();

        assert_eq!(find_tgproxy_in(None, Some(&path_var)), Some(expected));
    }

    #[test]
    fn find_tgproxy_reports_non_executable_sibling_before_path_copy() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }

        let sibling_dir = tempfile::tempdir().unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        let sibling = make_tgproxy(sibling_dir.path(), 0o644);
        make_tgproxy(path_dir.path(), 0o755);
        let path_var = std::env::join_paths([path_dir.path()]).unwrap();

        let found = find_tgproxy_in(Some(sibling_dir.path()), Some(&path_var)).unwrap();
        assert_eq!(found, sibling);
        assert_eq!(
            tgproxy_status_line(Some(&found)),
            format!("{} (present but not executable)", sibling.display()),
            "doctor must identify the unusable sibling the runtime selects"
        );
    }

    #[test]
    fn find_tgproxy_is_none_when_absent_or_not_executable() {
        let directory = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([directory.path()]).unwrap();

        if crate::platform::sound_process::test_support::UNIX {
            make_tgproxy(directory.path(), 0o644);
            assert_eq!(find_tgproxy_in(None, Some(&path_var)), None);
            std::fs::remove_file(directory.path().join("tgproxy")).unwrap();
        }

        std::fs::create_dir(directory.path().join("tgproxy")).unwrap();
        assert_eq!(find_tgproxy_in(None, Some(&path_var)), None);
        assert_eq!(find_tgproxy_in(None, None), None);
    }

    #[test]
    fn tgproxy_status_line_names_the_path_or_says_not_installed() {
        // A real executable file (the running test binary): the status line names its path.
        let exe = std::env::current_exe().unwrap();
        let found = exe.as_path();
        assert_eq!(
            tgproxy_status_line(Some(found)),
            found.display().to_string()
        );

        let absent = tgproxy_status_line(None);
        assert!(absent.contains("not installed"));
        assert!(absent.contains("cargo install --path crates/thegn-proxy"));
    }

    #[test]
    fn supervisor_step_stops_a_proxy_disabled_by_reload() {
        let desired = ModelProxyConfig::default();
        let applied = ModelProxyConfig {
            enabled: true,
            ..ModelProxyConfig::default()
        };

        assert_eq!(
            supervisor_step(&desired, Some(&applied)),
            SupervisorStep::Stop
        );
    }

    #[test]
    fn supervisor_step_restarts_on_a_changed_config() {
        let desired = ModelProxyConfig {
            enabled: true,
            listen: "127.0.0.1:47999".into(),
            ..ModelProxyConfig::default()
        };
        let applied = ModelProxyConfig {
            enabled: true,
            ..ModelProxyConfig::default()
        };

        assert_eq!(
            supervisor_step(&desired, Some(&applied)),
            SupervisorStep::Restart
        );
    }

    #[test]
    fn supervisor_step_ensures_an_unchanged_enabled_config() {
        let desired = ModelProxyConfig {
            enabled: true,
            ..ModelProxyConfig::default()
        };
        let applied = desired.clone();

        assert_eq!(
            supervisor_step(&desired, Some(&applied)),
            SupervisorStep::Ensure
        );
        assert_eq!(supervisor_step(&desired, None), SupervisorStep::Ensure);
    }

    #[test]
    fn supervisor_step_idles_while_disabled_and_never_applied() {
        assert_eq!(
            supervisor_step(&ModelProxyConfig::default(), None),
            SupervisorStep::Idle
        );
    }

    #[test]
    fn supervisor_wait_has_no_timeout_only_for_idle() {
        assert_eq!(supervisor_wait(SupervisorStep::Idle, 0), None);
        assert_eq!(
            supervisor_wait(SupervisorStep::Ensure, 0),
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            supervisor_wait(SupervisorStep::Stop, 0),
            Some(Duration::from_secs(15))
        );
        let expected_backoff = thegn_core::backoff::calculate_backoff(
            thegn_core::backoff::ExhaustionKind::ServerError,
            2,
        )
        .min(Duration::from_secs(120));
        assert_eq!(
            supervisor_wait(SupervisorStep::Restart, 2),
            Some(expected_backoff)
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "spawn a short-lived child to verify supervisor reaping"
    )]
    fn reap_spawned_child_clears_an_exited_child() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn short-lived fixture child");
        let mut stdout = child.stdout.take().expect("capture fixture stdout");
        std::io::Read::read_to_end(&mut stdout, &mut Vec::new())
            .expect("wait for fixture child stdout to close");
        *SPAWNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);

        assert!(reap_spawned_child(), "one reap pass clears an exited child");
        assert!(
            SPAWNED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        );
    }

    #[test]
    fn desired_cell_publish_bumps_the_generation_and_keeps_the_latest() {
        let cell = DesiredCell::new();
        let first = ModelProxyConfig {
            enabled: true,
            ..ModelProxyConfig::default()
        };
        let second = ModelProxyConfig {
            enabled: true,
            listen: "127.0.0.1:47999".into(),
            ..ModelProxyConfig::default()
        };

        assert_eq!(cell.publish(&first), 1);
        assert_eq!(cell.publish(&second), 2);
        assert_eq!(cell.snapshot(), (2, Some(second)));
    }

    #[test]
    fn desired_cell_wait_returns_early_on_publish() {
        let cell = std::sync::Arc::new(DesiredCell::new());
        let publisher_cell = std::sync::Arc::clone(&cell);
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            publisher_cell.publish(&ModelProxyConfig::default());
        });
        let started = std::time::Instant::now();

        let woke = cell.wait_newer(0, Duration::from_secs(10));
        let elapsed = started.elapsed();
        publisher.join().unwrap();
        assert!(woke);
        assert!(elapsed < Duration::from_secs(5));
    }

    #[test]
    fn desired_cell_wait_times_out_without_a_publish() {
        let cell = DesiredCell::new();

        assert!(!cell.wait_newer(0, Duration::from_millis(20)));
    }
}
