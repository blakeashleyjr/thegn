//! Host-side adapter for the generic toolchain activation seam.
//!
//! This is deliberately the only host module that knows the external
//! toolchain executable.  Launches consume a small, identity-stamped cache;
//! resolving a cold cache is always a background operation.  The adapter is
//! therefore safe to call from hydration and launch-spec code without making
//! the event loop wait for a child process.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

use thegn_core::bundle::ResolvedEnv;
use thegn_core::config::Config;
use thegn_core::config_resolve::GatedRequest;
use thegn_core::devenv::Devshell;
use thegn_core::envplan::EnvRequirements;
use thegn_core::remote::GitLoc;
use thegn_core::repo_trust;
use thegn_core::store::RepoTrustStore;
use thegn_core::toolchain::MiseInject;
use thegn_core::toolchain_activation::{
    ActivationLayer, ActivationPlan, ActivationPolicy, ConfigSetIdentity, DetectedToolchainFiles,
    ProviderAnswer, ProviderContext, ProviderProbe, ProviderState, ToolchainProvider,
    compose_activation, config_set_identity, config_set_identity_from_bytes, mise_env_request,
};

const ORIGIN: &str = "mise";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OUTPUT: usize = 1024 * 1024;
const TARGET_DETECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedActivation {
    identity: ConfigSetIdentity,
    path_entries: Vec<String>,
    env: Vec<(String, String)>,
    #[serde(default)]
    missing_tools: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedRemote {
    key: String,
    /// The probe is name-only output from `DETECT_PROBE_SCRIPT`; it contains
    /// no target file contents or resolved environment values.
    probe: String,
    identity: Option<ConfigSetIdentity>,
    binary: bool,
    shims: Option<String>,
    layer: Option<CachedLayer>,
    #[serde(default)]
    missing_tools: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedLayer {
    path_entries: Vec<String>,
    env: Vec<(String, String)>,
}

impl CachedLayer {
    fn from_layer(layer: &ActivationLayer) -> Self {
        Self {
            path_entries: layer.path_entries.clone(),
            env: layer.env.clone(),
        }
    }

    fn into_layer(self) -> ActivationLayer {
        ActivationLayer::ready(ORIGIN, self.path_entries, self.env)
    }
}

fn cache_dir() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn/mise")
}

fn cache_path(identity: &ConfigSetIdentity) -> PathBuf {
    cache_dir().join(format!("{}.json", identity.hash))
}

fn remote_cache_key(worktree: &str, loc: &GitLoc) -> String {
    let mut hasher = Sha256::new();
    hasher.update(worktree.as_bytes());
    hasher.update([0]);
    match loc {
        GitLoc::Local(path) => {
            hasher.update(b"local\0");
            hasher.update(path.to_string_lossy().as_bytes());
        }
        GitLoc::Remote { ssh, path } => {
            hasher.update(b"ssh\0");
            hasher.update(ssh.host.as_bytes());
            hasher.update([0]);
            hasher.update(ssh.port.to_string().as_bytes());
            hasher.update([0]);
            hasher.update(path.as_bytes());
            hasher.update([0]);
            hasher.update(ssh.ssh_config.as_deref().unwrap_or_default().as_bytes());
            hasher.update([0]);
            hasher.update(ssh.jump_host.as_deref().unwrap_or_default().as_bytes());
            hasher.update([0]);
            hasher.update(ssh.identity.as_deref().unwrap_or_default().as_bytes());
            hasher.update([0]);
            for arg in &ssh.extra_args {
                hasher.update(arg.as_bytes());
                hasher.update([0]);
            }
        }
        GitLoc::Provider {
            control_prefix,
            path,
        } => {
            hasher.update(b"provider\0");
            for arg in control_prefix {
                hasher.update(arg.as_bytes());
                hasher.update([0]);
            }
            hasher.update(path.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

fn remote_cache_path(worktree: &str, loc: &GitLoc) -> PathBuf {
    cache_dir().join(format!("remote-{}.json", remote_cache_key(worktree, loc)))
}

fn in_flight() -> &'static Mutex<BTreeSet<String>> {
    static KEYS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    KEYS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

struct RefreshTarget {
    tx: UnboundedSender<crate::hydrate::RefreshKind>,
    waker: TerminalWaker,
}

fn refresh_target() -> &'static Mutex<Option<RefreshTarget>> {
    static TARGET: OnceLock<Mutex<Option<RefreshTarget>>> = OnceLock::new();
    TARGET.get_or_init(|| Mutex::new(None))
}

/// Install the compositor's existing model-refresh sink. Resolution workers
/// use this same channel as every other off-loop producer; the provider never
/// owns or creates a second event-loop notification path.
pub(crate) fn install_refresh(
    tx: UnboundedSender<crate::hydrate::RefreshKind>,
    waker: TerminalWaker,
) {
    if let Ok(mut target) = refresh_target().lock() {
        *target = Some(RefreshTarget { tx, waker });
    }
}

fn pulse_refresh() {
    if let Ok(target) = refresh_target().lock()
        && let Some(target) = target.as_ref()
    {
        let _ = target.tx.send(crate::hydrate::RefreshKind::Model);
        let _ = target.waker.wake();
    }
}

fn config_set(worktree: &Path) -> (DetectedToolchainFiles, Option<ConfigSetIdentity>) {
    let detected = thegn_core::envplan::detect_with_mise_env(
        worktree,
        std::env::var("MISE_ENV").ok().as_deref(),
    )
    .toolchain_files;
    let identity = config_set_identity(&worktree.to_string_lossy(), worktree, &detected);
    (detected, identity)
}

fn policy(cfg: &Config, approved: bool) -> ActivationPolicy {
    cfg.toolchain.mise.activation_policy(approved)
}

fn approvals_for(db: Option<&thegn_core::db::Db>, repo_root: &Path) -> Vec<String> {
    db.and_then(|db| db.repo_trust_approved(&repo_root.to_string_lossy()).ok())
        .unwrap_or_default()
}

/// The request shown by `repo trust` and the launch-time trust notification.
/// It contains only the config-set digest and relative file names.
pub(crate) fn pending_request(
    cfg: &Config,
    worktree: &Path,
    _repo_root: &Path,
    approved: &[String],
) -> Option<GatedRequest> {
    if !matches!(
        cfg.toolchain.mise.inject,
        MiseInject::Auto | MiseInject::Env
    ) {
        return None;
    }
    let (_, identity) = config_set(worktree);
    let request = mise_env_request(&identity?);
    (!repo_trust::is_approved(&request, approved)).then_some(request)
}

/// Return the target-derived trust request. Runtime callers use the cache-only
/// form so a trust notification cannot block the event loop; the CLI passes
/// `refresh = true` because it is already an off-loop, explicit user action.
pub(crate) fn pending_request_for_target(
    cfg: &Config,
    worktree: &str,
    repo_root: &Path,
    loc: &GitLoc,
    approved: &[String],
    refresh: bool,
) -> Option<GatedRequest> {
    if !matches!(
        cfg.toolchain.mise.inject,
        MiseInject::Auto | MiseInject::Env
    ) {
        return None;
    }
    if !loc.is_remote() {
        return pending_request(cfg, Path::new(worktree), repo_root, approved);
    }
    if refresh {
        let db_approved = approved.to_vec();
        resolve_remote_cache_sync(cfg, worktree, loc, db_approved).ok()?;
    }
    let identity = read_remote_cache(worktree, loc)?.identity;
    let request = mise_env_request(&identity?);
    (!repo_trust::is_approved(&request, approved)).then_some(request)
}

fn resolve_remote_cache_sync(
    cfg: &Config,
    worktree: &str,
    loc: &GitLoc,
    approved: Vec<String>,
) -> Result<(), String> {
    let key = remote_cache_key(worktree, loc);
    let (ok, probe) = target_script(
        loc,
        thegn_core::envplan::DETECT_PROBE_SCRIPT,
        TARGET_DETECT_TIMEOUT,
        64 * 1024,
    )?;
    if !ok {
        return Err("remote toolchain detection failed".into());
    }
    let requirements = thegn_core::envplan::detect_from_probe(&probe);
    let binary = remote_binary_available(loc);
    let identity = remote_identity(loc, worktree, &requirements.toolchain_files);
    let allowed = approved_identity(identity.as_ref(), &approved);
    let desired = policy(cfg, allowed);
    let layer = if binary && allowed && desired == ActivationPolicy::Environment {
        remote_env(loc).ok()
    } else {
        None
    };
    let missing_tools = if binary {
        target_script(
            loc,
            "mise ls --missing --json",
            Duration::from_secs(5),
            64 * 1024,
        )
        .ok()
        .and_then(|(ok, output)| ok.then(|| parse_missing_tools(&output)))
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    let reason = if !binary {
        Some("toolchain executable is not installed on the target".into())
    } else if identity.is_none() {
        Some("remote toolchain config changed or could not be read".into())
    } else if allowed && desired == ActivationPolicy::Environment && layer.is_none() {
        Some("remote toolchain environment resolver failed".into())
    } else {
        None
    };
    write_remote_cache(
        worktree,
        loc,
        &CachedRemote {
            key,
            probe,
            identity,
            binary,
            shims: remote_shims_dir(loc).map(|p| p.to_string_lossy().into_owned()),
            layer: layer.as_ref().map(CachedLayer::from_layer),
            missing_tools,
            reason,
        },
    )
}

fn shims_dir() -> Option<PathBuf> {
    let data = std::env::var_os("MISE_DATA_DIR").map(PathBuf::from);
    let data = data.or_else(|| {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .map(|p| p.join("mise"))
    });
    let data = data.or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|p| p.join(".local/share/mise"))
    })?;
    let shims = data.join("shims");
    shims.is_dir().then_some(shims)
}

fn executable_available() -> bool {
    thegn_core::util::which_path("mise").is_some()
}

/// Run a target command with a deadline and a bounded stdout capture. This is
/// the common transport for SSH/provider detection and activation; the launch
/// path therefore never falls back to an unbounded `Command::output` call.
#[expect(
    clippy::disallowed_methods,
    reason = "target command execution is bounded and only used by off-loop launch resolution"
)]
fn run_target_command(
    mut command: Command,
    timeout: Duration,
    max_output: usize,
) -> Result<(bool, String), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start target toolchain command: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("target toolchain command stdout unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 8192];
        while bytes.len() < max_output {
            let n = stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n.min(max_output - bytes.len())]);
        }
        bytes
    });
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("target toolchain command failed: {e}"))?
        {
            let bytes = reader
                .join()
                .map_err(|_| "target toolchain output reader panicked")?;
            let output = String::from_utf8(bytes)
                .map_err(|_| "target toolchain command returned invalid UTF-8")?;
            return Ok((status.success(), output));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("target toolchain command timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn target_script(
    loc: &GitLoc,
    script: &str,
    timeout: Duration,
    max_output: usize,
) -> Result<(bool, String), String> {
    run_target_command(loc.sh_command(script), timeout, max_output)
}

#[cfg(test)]
fn remote_requirements(loc: &GitLoc) -> Result<EnvRequirements, String> {
    let (ok, output) = target_script(
        loc,
        thegn_core::envplan::DETECT_PROBE_SCRIPT,
        TARGET_DETECT_TIMEOUT,
        64 * 1024,
    )?;
    ok.then(|| thegn_core::envplan::detect_from_probe(&output))
        .ok_or_else(|| "remote toolchain detection failed".into())
}

fn remote_file(loc: &GitLoc, relative: &str) -> Option<Vec<u8>> {
    let quoted = thegn_core::util::sh_quote(relative);
    let script = format!(
        "no_symlink_components() {{ path=\"$1\"; while [ \"$path\" != . ] && [ \"$path\" != / ] && [ -n \"$path\" ]; do [ ! -L \"$path\" ] || return 1; case \"$path\" in */*) path=\"${{path%/*}}\"; [ -n \"$path\" ] || path=.;; *) path=.;; esac; done; }}; if no_symlink_components {quoted} && [ -f {quoted} ] && [ ! -L {quoted} ]; then cat {quoted}; else exit 42; fi"
    );
    let (ok, output) = target_script(loc, &script, TARGET_DETECT_TIMEOUT, MAX_OUTPUT).ok()?;
    ok.then(|| output.into_bytes())
}

/// Derive trust from the files on the remote target, not from a local
/// placeholder worktree. The request remains the normal core `mise.env`
/// request, so local and remote approvals share one canonical identity format.
fn remote_identity(
    loc: &GitLoc,
    worktree_identity: &str,
    detected: &DetectedToolchainFiles,
) -> Option<ConfigSetIdentity> {
    let mut contents = detected
        .all_files()
        .into_iter()
        .map(|relative| Some((relative.clone(), remote_file(loc, &relative)?)))
        .collect::<Option<Vec<_>>>()?;
    if let Some(lock) = remote_file(loc, "mise.lock") {
        contents.push(("mise.lock".into(), lock));
    }
    config_set_identity_from_bytes(worktree_identity, &contents)
}

fn approved_identity(identity: Option<&ConfigSetIdentity>, approved: &[String]) -> bool {
    identity
        .map(mise_env_request)
        .is_some_and(|request| repo_trust::is_approved(&request, approved))
}

fn remote_binary_available(loc: &GitLoc) -> bool {
    target_script(
        loc,
        "command -v mise >/dev/null 2>&1",
        Duration::from_secs(5),
        1,
    )
    .is_ok_and(|(ok, _)| ok)
}

fn remote_shims_dir(loc: &GitLoc) -> Option<PathBuf> {
    let script = r#"
d="${MISE_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/mise}/shims"
[ -d "$d" ] && printf '%s' "$d"
"#;
    let (ok, output) = target_script(loc, script, Duration::from_secs(5), 4096).ok()?;
    (ok && !output.trim().is_empty()).then(|| PathBuf::from(output.trim()))
}

fn remote_env(loc: &GitLoc) -> Result<ActivationLayer, String> {
    let output = r#"
env -i \
  PATH="${PATH:-/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin}" \
  HOME="${HOME:-/root}" USER="${USER:-}" LANG="${LANG:-C}" \
  MISE_DATA_DIR="${MISE_DATA_DIR:-}" XDG_DATA_HOME="${XDG_DATA_HOME:-}" \
  MISE_ENV="${MISE_ENV:-}" mise env -s json
"#;
    let (ok, output) = target_script(loc, output, RESOLVE_TIMEOUT, MAX_OUTPUT)?;
    if !ok {
        return Err("remote toolchain environment resolver failed".into());
    }
    parse_env_output(&output).ok_or_else(|| "invalid remote resolver JSON".into())
}

fn cached_layer(layer: Option<CachedLayer>) -> Option<ActivationLayer> {
    layer.map(CachedLayer::into_layer)
}

fn read_cache_record(identity: &ConfigSetIdentity) -> Option<CachedActivation> {
    let path = cache_path(identity);
    if !crate::platform::is_owner_only(&path) {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let cached = serde_json::from_str::<CachedActivation>(&raw).ok()?;
    (cached.identity == *identity).then_some(cached)
}

fn read_remote_cache(worktree: &str, loc: &GitLoc) -> Option<CachedRemote> {
    let path = remote_cache_path(worktree, loc);
    if !crate::platform::is_owner_only(&path) {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let cached = serde_json::from_str::<CachedRemote>(&raw).ok()?;
    (cached.key == remote_cache_key(worktree, loc)).then_some(cached)
}

fn write_remote_cache(worktree: &str, loc: &GitLoc, cached: &CachedRemote) -> Result<(), String> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::platform::restrict_dir_owner_only(&dir);
    let path = remote_cache_path(worktree, loc);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(cached).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    crate::platform::restrict_file_owner_only(&tmp);
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

fn remote_cached_activation(
    cfg: &Config,
    worktree: &str,
    repo_root: &Path,
    loc: &GitLoc,
    db: Option<&thegn_core::db::Db>,
) -> ProviderAnswer {
    let Some(cached) = read_remote_cache(worktree, loc) else {
        return ProviderAnswer::Reserved {
            origin: ORIGIN.into(),
            reason: "remote toolchain state is resolving; using the safe base environment".into(),
        };
    };
    let requirements = thegn_core::envplan::detect_from_probe(&cached.probe);
    if requirements.toolchain_files.is_empty() {
        return ProviderAnswer::Reserved {
            origin: ORIGIN.into(),
            reason: "remote target has no declared toolchain".into(),
        };
    }
    if !cached.binary {
        return ProviderAnswer::Unavailable {
            origin: ORIGIN.into(),
            reason: "toolchain executable is not installed on the target".into(),
        };
    }
    let approved = db
        .map(|db| approvals_for(Some(db), repo_root))
        .unwrap_or_default();
    let allowed = approved_identity(cached.identity.as_ref(), &approved);
    let desired = policy(cfg, allowed);
    let provider = MiseProvider {
        binary: cached.binary,
        shims: cached.shims.map(PathBuf::from),
        cached: (desired == ActivationPolicy::Environment && allowed)
            .then(|| cached_layer(cached.layer.clone()))
            .flatten(),
    };
    provider.activate(&ProviderContext {
        worktree_identity: worktree.into(),
        detected: requirements.toolchain_files,
        policy: desired,
        config_approved: allowed,
    })
}

/// Detect the declarations in the selected OCI/provider target. This keeps
/// target-local facts together with the provider operation and is also used by
/// host provisioning, where the host worktree may only be a placeholder.
pub(crate) fn detect_on_target(
    runner: &thegn_svc::host::OciRunner,
    container: &str,
    target_worktree: &str,
) -> Result<EnvRequirements, String> {
    let script = format!(
        "cd {} 2>/dev/null && {}",
        thegn_core::util::sh_quote(target_worktree),
        thegn_core::envplan::DETECT_PROBE_SCRIPT
    );
    let (ok, output, _) = runner.exec_in_container(container, &script, TARGET_DETECT_TIMEOUT)?;
    ok.then(|| thegn_core::envplan::detect_from_probe(&output))
        .ok_or_else(|| "target toolchain detection failed".into())
}

fn target_file(
    runner: &thegn_svc::host::OciRunner,
    container: &str,
    target_worktree: &str,
    relative: &str,
) -> Option<Vec<u8>> {
    let quoted = thegn_core::util::sh_quote(relative);
    let script = format!(
        "cd {} 2>/dev/null && no_symlink_components() {{ path=\"$1\"; while [ \"$path\" != . ] && [ \"$path\" != / ] && [ -n \"$path\" ]; do [ ! -L \"$path\" ] || return 1; case \"$path\" in */*) path=\"${{path%/*}}\"; [ -n \"$path\" ] || path=.;; *) path=.;; esac; done; }}; if no_symlink_components {quoted} && [ -f {quoted} ] && [ ! -L {quoted} ]; then cat {quoted}; else exit 42; fi",
        thegn_core::util::sh_quote(target_worktree),
    );
    let (ok, output, _) = runner
        .exec_in_container(container, &script, TARGET_DETECT_TIMEOUT)
        .ok()?;
    ok.then(|| output.into_bytes())
}

fn target_identity(
    runner: &thegn_svc::host::OciRunner,
    container: &str,
    target_worktree: &str,
    worktree_identity: &str,
    detected: &DetectedToolchainFiles,
) -> Option<ConfigSetIdentity> {
    let mut contents = detected
        .all_files()
        .into_iter()
        .map(|relative| {
            Some((
                relative.clone(),
                target_file(runner, container, target_worktree, &relative)?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    if let Some(lock) = target_file(runner, container, target_worktree, "mise.lock") {
        contents.push(("mise.lock".into(), lock));
    }
    config_set_identity_from_bytes(worktree_identity, &contents)
}

/// Run the provider-owned install inside the selected target. This is the
/// explicit host-provisioning operation; normal launch activation never calls
/// it. Trust is checked against target-side bytes before any install command.
pub(crate) fn install_on_target(
    cfg: &Config,
    worktree_identity: &str,
    target_worktree: &str,
    repo_root: &Path,
    runner: &thegn_svc::host::OciRunner,
    container: &str,
    requirements: &EnvRequirements,
) -> Result<(), String> {
    if cfg.toolchain.mise.inject == MiseInject::Off {
        return Err("toolchain activation is off ([toolchain.mise] inject = \"off\")".into());
    }
    let Some(identity) = target_identity(
        runner,
        container,
        target_worktree,
        worktree_identity,
        &requirements.toolchain_files,
    ) else {
        return Err("target toolchain config changed or could not be read; install refused".into());
    };
    let db = thegn_core::db::Db::open()
        .map_err(|_| "repo trust state is unavailable; install refused".to_string())?;
    let approved = db
        .repo_trust_approved(&repo_root.to_string_lossy())
        .map_err(|_| "repo trust state is unavailable; install refused".to_string())?;
    if !approved_identity(Some(&identity), &approved) {
        return Err("target toolchain config is not approved; review `thegn repo trust`".into());
    }
    let (available, _, _) = runner
        .exec_in_container(
            container,
            "command -v mise >/dev/null 2>&1",
            Duration::from_secs(5),
        )
        .map_err(|_| "toolchain executable is not installed on the target".to_string())?;
    if !available {
        return Err("toolchain executable is not installed on the target".into());
    }
    let script = format!(
        "cd {} 2>/dev/null && env -i PATH=\"${{PATH:-/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin}}\" HOME=\"${{HOME:-/root}}\" USER=\"${{USER:-}}\" LANG=\"${{LANG:-C}}\" MISE_DATA_DIR=\"${{MISE_DATA_DIR:-}}\" XDG_DATA_HOME=\"${{XDG_DATA_HOME:-}}\" MISE_ENV=\"${{MISE_ENV:-}}\" mise install",
        thegn_core::util::sh_quote(target_worktree),
    );
    let (ok, _, _) = runner
        .exec_in_container(container, &script, INSTALL_TIMEOUT)
        .map_err(|e| format!("target toolchain install failed: {e}"))?;
    ok.then_some(())
        .ok_or_else(|| "target toolchain install failed".into())
}

/// Install the active worktree's declared tools after the user has explicitly
/// approved its current config set. This is intentionally the only install
/// entry point: launch activation uses shims/cache and never calls it.
#[expect(
    clippy::disallowed_methods,
    reason = "the bounded install child runs on an explicit off-loop worker"
)]
pub(crate) fn install(cfg: &Config, worktree: &Path, repo_root: &Path) -> Result<(), String> {
    if !worktree.is_dir() {
        return Err("active worktree is unavailable".into());
    }
    if GitLoc::for_worktree(worktree).is_remote() {
        return Err("selected worktree is remote; install its toolchain there".into());
    }
    if cfg.toolchain.mise.inject == MiseInject::Off {
        return Err("toolchain activation is off ([toolchain.mise] inject = \"off\")".into());
    }
    let (detected, identity) = config_set(worktree);
    if detected.is_empty() {
        return Err("no declared worktree toolchain found".into());
    }
    let Some(identity) = identity else {
        return Err("worktree toolchain config changed; retry install".into());
    };
    let db = thegn_core::db::Db::open()
        .map_err(|_| "repo trust state is unavailable; install refused".to_string())?;
    let approved = db
        .repo_trust_approved(&repo_root.to_string_lossy())
        .map_err(|_| "repo trust state is unavailable; install refused".to_string())?;
    let request = mise_env_request(&identity);
    if !repo_trust::is_approved(&request, &approved) {
        return Err("worktree toolchain config is not approved; review `thegn repo trust`".into());
    }
    if !executable_available() {
        return Err("toolchain executable is not installed".into());
    }

    let mut child = Command::new("mise")
        .arg("install")
        .current_dir(worktree)
        .env_clear()
        .envs(std::env::vars().filter(|(key, _)| {
            matches!(
                key.as_str(),
                "PATH" | "HOME" | "USER" | "LANG" | "MISE_DATA_DIR" | "XDG_DATA_HOME" | "MISE_ENV"
            )
        }))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start toolchain install: {e}"))?;
    let deadline = Instant::now() + INSTALL_TIMEOUT;
    loop {
        if child
            .try_wait()
            .map_err(|e| format!("toolchain install failed: {e}"))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("toolchain install timed out".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let status = child
        .wait()
        .map_err(|e| format!("toolchain install failed: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "toolchain install failed".into())
}

fn read_cache(identity: &ConfigSetIdentity) -> Option<ActivationLayer> {
    let cached = read_cache_record(identity)?;
    cached
        .reason
        .is_none()
        .then(|| ActivationLayer::ready(ORIGIN, cached.path_entries, cached.env))
}

fn write_cache(
    identity: &ConfigSetIdentity,
    layer: Option<ActivationLayer>,
    missing_tools: Vec<String>,
    reason: Option<String>,
) -> Result<(), String> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::platform::restrict_dir_owner_only(&dir);
    let path = cache_path(identity);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(&CachedActivation {
        identity: identity.clone(),
        path_entries: layer
            .as_ref()
            .map(|layer| layer.path_entries.clone())
            .unwrap_or_default(),
        env: layer.map(|layer| layer.env).unwrap_or_default(),
        missing_tools,
        reason,
    })
    .map_err(|e| e.to_string())?;
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    crate::platform::restrict_file_owner_only(&tmp);
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

fn safe_env_key(key: &str) -> bool {
    !key.is_empty()
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !matches!(
            key,
            "HOME" | "PWD" | "OLDPWD" | "SHELL" | "USER" | "LOGNAME"
        )
}

/// Parse the machine-readable environment response without retaining shell
/// syntax or command output.  Both current object output and the older
/// `{variables: {KEY: {value: ...}}}` shape are accepted.
fn parse_env_output(raw: &str) -> Option<ActivationLayer> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let object = value.as_object()?;
    let vars = object
        .get("variables")
        .and_then(serde_json::Value::as_object)
        .unwrap_or(object);
    let mut env = BTreeMap::new();
    let mut paths = Vec::new();
    for (key, value) in vars {
        if key == "_.path" || key == "_" {
            if let Some(path) = value.as_str() {
                paths.extend(std::env::split_paths(path).map(|p| p.to_string_lossy().into_owned()));
            } else if let Some(path) = value.get("value").and_then(|v| v.as_str()) {
                paths.extend(std::env::split_paths(path).map(|p| p.to_string_lossy().into_owned()));
            }
            continue;
        }
        let value = value
            .as_str()
            .or_else(|| value.get("value").and_then(|v| v.as_str()));
        if safe_env_key(key) && key != "PATH" {
            if let Some(value) = value {
                env.insert(key.clone(), value.to_string());
            }
        } else if key == "PATH"
            && let Some(value) = value
        {
            paths.extend(std::env::split_paths(value).map(|p| p.to_string_lossy().into_owned()));
        }
    }
    Some(ActivationLayer::ready(
        ORIGIN,
        paths,
        env.into_iter().collect(),
    ))
}

#[expect(
    clippy::disallowed_methods,
    reason = "the bounded resolver child runs on an explicit off-loop worker"
)]
fn run_env(worktree: &Path) -> Result<String, String> {
    let mut child = Command::new("mise")
        .args(["env", "-s", "json"])
        .current_dir(worktree)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| {
            matches!(
                k.as_str(),
                "PATH" | "HOME" | "USER" | "LANG" | "MISE_DATA_DIR" | "XDG_DATA_HOME" | "MISE_ENV"
            )
        }))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start toolchain resolver: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("resolver stdout unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 8192];
        while bytes.len() < MAX_OUTPUT {
            let n = stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n.min(MAX_OUTPUT - bytes.len())]);
        }
        bytes
    });
    let deadline = Instant::now() + RESOLVE_TIMEOUT;
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("toolchain environment resolution timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let bytes = reader
        .join()
        .map_err(|_| "resolver output reader panicked")?;
    if !status.success() {
        return Err("toolchain environment resolver failed".into());
    }
    String::from_utf8(bytes).map_err(|_| "toolchain resolver returned invalid UTF-8".into())
}

/// Run a bounded, non-interactive informational query. This is used only by
/// the CLI doctor path; hydration and launch use `status`/the cache above.
#[expect(
    clippy::disallowed_methods,
    reason = "the bounded doctor child runs on an explicit off-loop worker"
)]
fn run_info(worktree: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new("mise")
        .args(args)
        .current_dir(worktree)
        .env_clear()
        .envs(std::env::vars().filter(|(key, _)| {
            matches!(
                key.as_str(),
                "PATH" | "HOME" | "USER" | "LANG" | "MISE_DATA_DIR" | "XDG_DATA_HOME" | "MISE_ENV"
            )
        }))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("informational query stdout unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        while bytes.len() < 16 * 1024 {
            let n = stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n.min(16 * 1024 - bytes.len())]);
        }
        bytes
    });
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("informational query timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let success = child.wait().map_err(|e| e.to_string())?.success();
    let output = reader.join().map_err(|_| "informational reader panicked")?;
    if !success {
        return Err("informational query failed".into());
    }
    String::from_utf8(output).map_err(|_| "informational query returned invalid UTF-8".into())
}

fn parse_missing_tools(raw: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        let mut values = vec![&value];
        while let Some(value) = values.pop() {
            match value {
                serde_json::Value::Array(items) => values.extend(items),
                serde_json::Value::Object(object) => {
                    if object.get("installed").and_then(|v| v.as_bool()) == Some(false)
                        || object.get("missing").and_then(|v| v.as_bool()) == Some(true)
                    {
                        for key in ["name", "tool", "plugin"] {
                            if let Some(name) = object.get(key).and_then(|v| v.as_str()) {
                                names.insert(name.to_string());
                            }
                        }
                    }
                    values.extend(object.values());
                }
                _ => {}
            }
        }
    }
    names.into_iter().collect()
}

fn resolve_cached(worktree: &Path, identity: &ConfigSetIdentity) {
    let result = run_env(worktree)
        .and_then(|raw| parse_env_output(&raw).ok_or_else(|| "invalid resolver JSON".to_string()))
        .and_then(|layer| {
            let missing_tools = run_info(
                worktree,
                &["ls", "--missing", "--json"],
                Duration::from_secs(5),
            )
            .map(|raw| parse_missing_tools(&raw))
            .unwrap_or_default();
            write_cache(identity, Some(layer), missing_tools, None)
        });
    if let Err(reason) = result {
        let _ = write_cache(identity, None, Vec::new(), Some(reason.clone()));
        tracing::debug!(target: "thegn::toolchain", %reason, "toolchain environment unavailable");
    }
    pulse_refresh();
    if let Ok(mut keys) = in_flight().lock() {
        keys.remove(&identity.hash);
    }
}

fn prewarm(worktree: &Path, identity: &ConfigSetIdentity) {
    if read_cache_record(identity).is_some() || !executable_available() {
        return;
    }
    let Ok(mut keys) = in_flight().lock() else {
        return;
    };
    if !keys.insert(identity.hash.clone()) {
        return;
    }
    let worktree = worktree.to_path_buf();
    let identity = identity.clone();
    let key = identity.hash.clone();
    if std::thread::Builder::new()
        .name("thegn-toolchain".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            resolve_cached(&worktree, &identity);
        })
        .is_err()
    {
        // A failed worker spawn must not permanently suppress retries for this
        // identity. Resource exhaustion is transient and the next launch can
        // succeed once another worker exits.
        if let Ok(mut keys) = in_flight().lock() {
            keys.remove(&key);
        }
    }
}

fn resolve_remote_cache(cfg: &Config, worktree: &str, loc: &GitLoc, approved: Vec<String>) {
    let key = remote_cache_key(worktree, loc);
    let result = target_script(
        loc,
        thegn_core::envplan::DETECT_PROBE_SCRIPT,
        TARGET_DETECT_TIMEOUT,
        64 * 1024,
    )
    .and_then(|(ok, probe)| {
        if !ok {
            return Err("remote toolchain detection failed".into());
        }
        let requirements = thegn_core::envplan::detect_from_probe(&probe);
        let binary = remote_binary_available(loc);
        let identity = remote_identity(loc, worktree, &requirements.toolchain_files);
        let allowed = approved_identity(identity.as_ref(), &approved);
        let desired = policy(cfg, allowed);
        let layer = if binary && allowed && desired == ActivationPolicy::Environment {
            remote_env(loc).ok()
        } else {
            None
        };
        let missing_tools = if binary {
            target_script(
                loc,
                "mise ls --missing --json",
                Duration::from_secs(5),
                64 * 1024,
            )
            .ok()
            .and_then(|(ok, output)| ok.then(|| parse_missing_tools(&output)))
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        let reason = if !binary {
            Some("toolchain executable is not installed on the target".into())
        } else if identity.is_none() {
            Some("remote toolchain config changed or could not be read".into())
        } else if allowed && desired == ActivationPolicy::Environment && layer.is_none() {
            Some("remote toolchain environment resolver failed".into())
        } else {
            None
        };
        let cached = CachedRemote {
            key: key.clone(),
            probe,
            identity,
            binary,
            shims: remote_shims_dir(loc).map(|p| p.to_string_lossy().into_owned()),
            layer: layer.as_ref().map(CachedLayer::from_layer),
            missing_tools,
            reason,
        };
        Ok((requirements, cached))
    });
    let result = result.and_then(|(_, cached)| write_remote_cache(worktree, loc, &cached));
    if let Err(reason) = result {
        let cached = CachedRemote {
            key: key.clone(),
            probe: String::new(),
            identity: None,
            binary: false,
            shims: None,
            layer: None,
            missing_tools: Vec::new(),
            reason: Some(reason),
        };
        let _ = write_remote_cache(worktree, loc, &cached);
    }
    pulse_refresh();
    if let Ok(mut keys) = in_flight().lock() {
        keys.remove(&format!("remote:{key}"));
    }
}

fn prewarm_remote(
    cfg: &Config,
    worktree: &str,
    repo_root: &Path,
    loc: &GitLoc,
    db: Option<&thegn_core::db::Db>,
) {
    let key = format!("remote:{}", remote_cache_key(worktree, loc));
    if let Ok(mut keys) = in_flight().lock() {
        if !keys.insert(key.clone()) {
            return;
        }
    } else {
        return;
    }
    let cfg = cfg.clone();
    let worktree = worktree.to_string();
    let repo_root = repo_root.to_path_buf();
    let loc = loc.clone();
    let approved = db
        .map(|db| approvals_for(Some(db), repo_root.as_path()))
        .unwrap_or_default();
    if std::thread::Builder::new()
        .name("thegn-toolchain-remote".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            resolve_remote_cache(&cfg, &worktree, &loc, approved);
        })
        .is_err()
    {
        // Keep the remote resolver retryable when the OS refuses a new worker.
        if let Ok(mut keys) = in_flight().lock() {
            keys.remove(&key);
        }
    }
}

struct MiseProvider {
    binary: bool,
    shims: Option<PathBuf>,
    cached: Option<ActivationLayer>,
}

impl ToolchainProvider for MiseProvider {
    fn kind(&self) -> &'static str {
        ORIGIN
    }

    fn probe(&self, context: &ProviderContext) -> ProviderProbe {
        if context.detected.is_empty() {
            return ProviderProbe {
                origin: ORIGIN.into(),
                status: ProviderState::Reserved,
                version: None,
                reason: None,
            };
        }
        if !self.binary {
            return ProviderProbe {
                origin: ORIGIN.into(),
                status: ProviderState::Unavailable,
                version: None,
                reason: Some("toolchain executable is not installed".into()),
            };
        }
        ProviderProbe {
            origin: ORIGIN.into(),
            status: ProviderState::Ready,
            version: None,
            reason: None,
        }
    }

    fn activate(&self, context: &ProviderContext) -> ProviderAnswer {
        if context.policy == ActivationPolicy::Off || context.detected.is_empty() {
            return ProviderAnswer::Reserved {
                origin: ORIGIN.into(),
                reason: "no toolchain activation requested".into(),
            };
        }
        if !self.binary {
            return ProviderAnswer::Unavailable {
                origin: ORIGIN.into(),
                reason: "toolchain executable is not installed".into(),
            };
        }
        let mut paths = self
            .shims
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut env = Vec::new();
        if context.policy == ActivationPolicy::Environment
            && let Some(layer) = &self.cached
        {
            paths.extend(layer.path_entries.iter().cloned());
            env.extend(layer.env.iter().cloned());
        }
        ProviderAnswer::Ready(ActivationLayer::ready(ORIGIN, paths, env))
    }
}

/// Compose the cache-backed plan used by every launch form. This function does
/// no child-process work; a cold approved environment is scheduled for the
/// next launch and receives shims in the meantime.
pub(crate) fn activation_for_launch(
    cfg: &Config,
    worktree: &str,
    repo_root: &Path,
    loc: &GitLoc,
    bundle: &ResolvedEnv,
    devshell: Option<&Devshell>,
    db: Option<&thegn_core::db::Db>,
) -> ActivationPlan {
    let approved = db
        .map(|db| approvals_for(Some(db), repo_root))
        .unwrap_or_default();
    if loc.is_remote() {
        // Target detection, identity, probes, and approved env resolution are
        // all transport operations. Refresh them off-loop, while this launch
        // consumes only the last validated target record (or a Reserved/safe
        // base answer on a cold cache).
        prewarm_remote(cfg, worktree, repo_root, loc, db);
        let answer = remote_cached_activation(cfg, worktree, repo_root, loc, db);
        return compose_activation(bundle, devshell, &[answer], None);
    }
    // A remote/provider worktree's path is target-local (or may merely be a
    // registry placeholder on this host). Never inspect that host path to
    // derive a mise trust decision; the remote target must provide its own
    // detection/identity through the provider boundary.
    let approved = pending_request(cfg, Path::new(worktree), repo_root, &approved).is_none();
    let requirements = thegn_core::envplan::detect_with_mise_env(
        Path::new(worktree),
        std::env::var("MISE_ENV").ok().as_deref(),
    );
    let detected = requirements.toolchain_files.clone();
    let identity = config_set_identity(worktree, Path::new(worktree), &detected);
    let desired = policy(cfg, approved);
    let cached = identity.as_ref().and_then(read_cache);
    if desired == ActivationPolicy::Environment
        && approved
        && let Some(identity) = &identity
        && cached.is_none()
    {
        prewarm(Path::new(worktree), identity);
    }
    let provider = MiseProvider {
        binary: executable_available(),
        shims: shims_dir(),
        cached,
    };
    let context = ProviderContext {
        worktree_identity: worktree.to_string(),
        detected,
        policy: desired,
        config_approved: approved,
    };
    let answer = provider.activate(&context);
    compose_activation(bundle, devshell, &[answer], None)
}

/// The host-visible status is presence-only and cache-only. It is suitable for
/// hydration, where probing a child process would violate the frame budget.
pub(crate) fn status(
    cfg: &Config,
    worktree: &Path,
    repo_root: &Path,
    db: Option<&thegn_core::db::Db>,
) -> ToolchainStatus {
    let loc = GitLoc::for_worktree(worktree);
    if loc.is_remote() {
        let Some(cached) = read_remote_cache(&worktree.to_string_lossy(), &loc) else {
            return ToolchainStatus {
                provider: ORIGIN.into(),
                tier: "Reserved".into(),
                inject: cfg.toolchain.mise.inject.as_str().into(),
                state: "remote".into(),
                reason: Some("toolchain state belongs to the remote target".into()),
                trust: "not-applicable".into(),
                ..ToolchainStatus::default()
            };
        };
        let requirements = thegn_core::envplan::detect_from_probe(&cached.probe);
        let approved = db
            .map(|db| approvals_for(Some(db), repo_root))
            .unwrap_or_default();
        let pending = cached
            .identity
            .as_ref()
            .map(mise_env_request)
            .is_some_and(|request| !repo_trust::is_approved(&request, &approved));
        let state = if requirements.toolchain_files.is_empty()
            || cfg.toolchain.mise.inject == MiseInject::Off
        {
            "off"
        } else if !cached.binary {
            "missing-binary"
        } else if pending || cached.identity.is_none() {
            "pending-trust"
        } else if cached.reason.is_some() {
            "degraded"
        } else if !cached.missing_tools.is_empty() {
            "missing-tools"
        } else if cached.layer.is_some() {
            "ready"
        } else {
            "shims"
        };
        return ToolchainStatus {
            provider: ORIGIN.into(),
            tier: format!("{:?}", requirements.tier()),
            inject: cfg.toolchain.mise.inject.as_str().into(),
            state: state.into(),
            files: requirements.toolchain_files.all_files(),
            shims: cached.shims,
            reason: cached.reason,
            version: None,
            trust: if requirements.toolchain_files.is_empty() {
                "not-applicable".into()
            } else if pending || cached.identity.is_none() {
                "pending".into()
            } else {
                "approved".into()
            },
            missing_tools: cached.missing_tools,
        };
    }
    let requirements = thegn_core::envplan::detect_with_mise_env(
        worktree,
        std::env::var("MISE_ENV").ok().as_deref(),
    );
    let identity = config_set_identity(
        &worktree.to_string_lossy(),
        worktree,
        &requirements.toolchain_files,
    );
    let approved = db
        .map(|db| approvals_for(Some(db), repo_root))
        .unwrap_or_default();
    let pending = pending_request(cfg, worktree, repo_root, &approved).is_some();
    let mode = cfg.toolchain.mise.inject.as_str().to_string();
    let cached = identity.as_ref().and_then(read_cache_record);
    let state = if requirements.toolchain_files.is_empty()
        || cfg.toolchain.mise.inject == MiseInject::Off
    {
        "off"
    } else if !executable_available() {
        "missing-binary"
    } else if pending {
        "pending-trust"
    } else if shims_dir().is_none() {
        "missing-shims"
    } else if cached.as_ref().is_some_and(|cache| cache.reason.is_some()) {
        "degraded"
    } else if cached
        .as_ref()
        .is_some_and(|cache| !cache.missing_tools.is_empty())
    {
        "missing-tools"
    } else if cached.is_some() {
        "ready"
    } else {
        "shims"
    };
    ToolchainStatus {
        provider: ORIGIN.into(),
        tier: format!("{:?}", requirements.tier()),
        inject: mode,
        state: state.into(),
        files: requirements.toolchain_files.all_files(),
        shims: shims_dir().map(|p| p.display().to_string()),
        reason: cached
            .as_ref()
            .and_then(|cache| cache.reason.clone())
            .or_else(|| {
                matches!(state, "missing-binary" | "missing-shims").then(|| {
                    if state == "missing-shims" {
                        "toolchain shims directory is not available".into()
                    } else {
                        "toolchain executable is not installed".into()
                    }
                })
            }),
        version: None,
        trust: if requirements.toolchain_files.is_empty() {
            "not-applicable".into()
        } else if pending {
            "pending".into()
        } else {
            "approved".into()
        },
        missing_tools: cached.map(|cache| cache.missing_tools).unwrap_or_default(),
    }
}

/// Add informational version/missing-tool facts for the synchronous doctor
/// command. Child output is parsed for names only and is never printed raw.
pub(crate) fn doctor_status(
    cfg: &Config,
    worktree: &Path,
    repo_root: &Path,
    db: Option<&thegn_core::db::Db>,
) -> ToolchainStatus {
    let mut status = status(cfg, worktree, repo_root, db);
    if matches!(status.state.as_str(), "off" | "remote") || !executable_available() {
        return status;
    }
    if let Ok(version) = run_info(worktree, &["--version"], Duration::from_secs(2)) {
        status.version = version
            .lines()
            .next()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
    }
    if let Ok(missing) = run_info(
        worktree,
        &["ls", "--missing", "--json"],
        Duration::from_secs(5),
    ) {
        status.missing_tools = parse_missing_tools(&missing);
    }
    status
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ToolchainStatus {
    pub provider: String,
    pub tier: String,
    pub inject: String,
    pub state: String,
    pub files: Vec<String>,
    pub shims: Option<String>,
    pub reason: Option<String>,
    pub version: Option<String>,
    pub trust: String,
    pub missing_tools: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_filters_shell_state() {
        let layer =
            parse_env_output(r#"{"PATH":"/a:/b","HOME":"bad","RUST_LOG":"info","_.path":"/shim"}"#)
                .unwrap();
        assert_eq!(layer.path_entries, vec!["/a", "/b", "/shim"]);
        assert_eq!(layer.env, vec![("RUST_LOG".into(), "info".into())]);
    }

    #[test]
    fn malformed_environment_is_unavailable_to_cache() {
        assert!(parse_env_output("not json").is_none());
        assert!(parse_env_output("[]").is_none());
    }

    #[test]
    fn trust_request_is_redacted_and_changes_with_approval() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mise.toml"), "[env]\nTOKEN='secret'\n").unwrap();
        let cfg = Config::default();
        let request = pending_request(&cfg, dir.path(), dir.path(), &[]).unwrap();
        let canonical = request.canonical();
        assert!(canonical.contains("mise.env"));
        assert!(canonical.contains("mise.toml"));
        assert!(!canonical.contains("secret"));
        assert!(pending_request(&cfg, dir.path(), dir.path(), &[canonical]).is_none());
    }

    #[test]
    fn missing_tool_parser_returns_stable_names_only() {
        let tools = parse_missing_tools(
            r#"[{"name":"node","installed":false},{"tool":"python","missing":true},{"name":"ok","installed":true}]"#,
        );
        assert_eq!(tools, vec!["node", "python"]);
    }

    #[test]
    fn cache_round_trip_is_identity_stamped() {
        let identity = ConfigSetIdentity {
            hash: "abc".into(),
            files: vec!["mise.toml".into()],
        };
        let layer = ActivationLayer::ready(ORIGIN, vec!["/shim".into()], vec![]);
        let cached = CachedActivation {
            identity: identity.clone(),
            path_entries: layer.path_entries.clone(),
            env: layer.env.clone(),
            missing_tools: Vec::new(),
            reason: None,
        };
        let raw = serde_json::to_string(&cached).unwrap();
        let restored = serde_json::from_str::<CachedActivation>(&raw).unwrap();
        assert_eq!(restored.identity, identity);
        assert_eq!(restored.path_entries, layer.path_entries);
        assert_eq!(restored.env, layer.env);
    }

    #[test]
    fn target_detection_and_identity_use_target_worktree_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mise.toml"), "[tools]\nnode='20'\n").unwrap();
        let loc = GitLoc::Local(dir.path().to_path_buf());
        let requirements = remote_requirements(&loc).unwrap();
        assert_eq!(requirements.toolchain_files.all_files(), vec!["mise.toml"]);
        let identity = remote_identity(&loc, "host-placeholder", &requirements.toolchain_files)
            .expect("target files should produce an identity");
        assert_eq!(identity.files, vec!["mise.toml"]);
        assert!(!identity.hash.is_empty());
    }
}
