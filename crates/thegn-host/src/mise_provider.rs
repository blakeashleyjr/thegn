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
    compose_activation, config_set_identity, mise_env_request,
};

const ORIGIN: &str = "mise";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OUTPUT: usize = 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedActivation {
    identity: ConfigSetIdentity,
    path_entries: Vec<String>,
    env: Vec<(String, String)>,
}

fn cache_dir() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn/mise")
}

fn cache_path(identity: &ConfigSetIdentity) -> PathBuf {
    cache_dir().join(format!("{}.json", identity.hash))
}

fn in_flight() -> &'static Mutex<BTreeSet<String>> {
    static KEYS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    KEYS.get_or_init(|| Mutex::new(BTreeSet::new()))
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
    let path = cache_path(identity);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).ok()?.permissions().mode();
        if mode & 0o077 != 0 {
            return None;
        }
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let cached = serde_json::from_str::<CachedActivation>(&raw).ok()?;
    (cached.identity == *identity)
        .then(|| ActivationLayer::ready(ORIGIN, cached.path_entries, cached.env))
}

fn write_cache(identity: &ConfigSetIdentity, layer: ActivationLayer) -> Result<(), String> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let path = cache_path(identity);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(&CachedActivation {
        identity: identity.clone(),
        path_entries: layer.path_entries,
        env: layer.env,
    })
    .map_err(|e| e.to_string())?;
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
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
        .and_then(|layer| write_cache(identity, layer));
    if let Err(reason) = result {
        tracing::debug!(target: "thegn::toolchain", %reason, "toolchain environment unavailable");
    }
    if let Ok(mut keys) = in_flight().lock() {
        keys.remove(&identity.hash);
    }
}

fn prewarm(worktree: &Path, identity: &ConfigSetIdentity) {
    if read_cache(identity).is_some() || !executable_available() {
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
    std::thread::Builder::new()
        .name("thegn-toolchain".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            resolve_cached(&worktree, &identity);
        })
        .ok();
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
    let approved = pending_request(cfg, Path::new(worktree), repo_root, &approved).is_none();
    let requirements = if loc.is_remote() {
        EnvRequirements::default()
    } else {
        thegn_core::envplan::detect_with_mise_env(
            Path::new(worktree),
            std::env::var("MISE_ENV").ok().as_deref(),
        )
    };
    let detected = requirements.toolchain_files.clone();
    let identity = (!loc.is_remote())
        .then(|| config_set_identity(worktree, Path::new(worktree), &detected))
        .flatten();
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
    let answer = if loc.is_remote() {
        ProviderAnswer::Reserved {
            origin: ORIGIN.into(),
            reason: "target resolves outside the local host".into(),
        }
    } else {
        provider.activate(&context)
    };
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
    } else if identity.as_ref().and_then(read_cache).is_some() {
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
        reason: matches!(state, "missing-binary" | "missing-shims").then(|| {
            if state == "missing-shims" {
                "toolchain shims directory is not available".into()
            } else {
                "toolchain executable is not installed".into()
            }
        }),
        version: None,
        trust: if requirements.toolchain_files.is_empty() {
            "not-applicable".into()
        } else if pending {
            "pending".into()
        } else {
            "approved".into()
        },
        missing_tools: Vec::new(),
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
    if status.state == "off" || !executable_available() {
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
        };
        let raw = serde_json::to_string(&cached).unwrap();
        let restored = serde_json::from_str::<CachedActivation>(&raw).unwrap();
        assert_eq!(restored.identity, identity);
        assert_eq!(restored.path_entries, layer.path_entries);
        assert_eq!(restored.env, layer.env);
    }
}
