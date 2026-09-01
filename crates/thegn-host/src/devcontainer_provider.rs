//! Host-owned provider seam for the optional `devcontainer` CLI.
//!
//! The core only discovers, parses, classifies, and trust-gates a
//! `devcontainer.json`. This module owns the process boundary: executable
//! discovery, bounded version probing, `up`, and `exec` argv construction.
//! Callers receive an opaque session and never need to know CLI-specific flags.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest as Sha2Digest, Sha256};

const CLI_NAME: &str = "devcontainer";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The provider's bounded capability result, suitable for doctor and status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeReport {
    pub state: ProbeState,
    pub executable: Option<String>,
    pub version: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeState {
    Ready,
    Unavailable,
    Degraded,
}

/// The transient status shown beside the existing environment token. This is
/// deliberately a value, not persisted state: it is recomputed from the repo
/// file, trust approvals, and the optional provider probe during hydration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DevcontainerStatus {
    pub variant: String,
    pub state: DevcontainerState,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DevcontainerState {
    Off,
    Ambiguous,
    Invalid,
    Pending,
    Ready,
    Degraded,
}

impl DevcontainerStatus {
    pub(crate) fn token(&self) -> String {
        let label = match self.state {
            DevcontainerState::Off => "off",
            DevcontainerState::Ambiguous => "ambiguous",
            DevcontainerState::Invalid => "invalid",
            DevcontainerState::Pending => "pending",
            DevcontainerState::Ready => "ready",
            DevcontainerState::Degraded => "degraded",
        };
        if self.variant.is_empty() {
            format!("dc:[{label}]")
        } else {
            format!("dc:{} [{label}]", self.variant)
        }
    }

    pub(crate) fn state_label(&self) -> &'static str {
        match self.state {
            DevcontainerState::Off => "off",
            DevcontainerState::Ambiguous => "ambiguous",
            DevcontainerState::Invalid => "invalid",
            DevcontainerState::Pending => "pending",
            DevcontainerState::Ready => "ready",
            DevcontainerState::Degraded => "degraded",
        }
    }
}

impl ProbeReport {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            state: ProbeState::Unavailable,
            executable: None,
            version: None,
            reason: Some(reason.into()),
        }
    }

    pub(crate) fn ready(&self) -> bool {
        self.state == ProbeState::Ready
    }
}

/// Whether the optional CLI can preserve the effective thegn sandbox policy.
///
/// `devcontainer up/exec` owns its own container arguments, so it cannot
/// reproduce thegn's hardening profile, network controls, VPN, or isolation
/// floor. Keep the provider branch deliberately narrow: any policy it cannot
/// represent falls through to the native OCI resolver, which is the
/// authoritative path for those settings.
pub(crate) fn can_honor_sandbox(sb: &thegn_core::config::SandboxConfig) -> bool {
    use thegn_core::config::{FileAccess, Network, SandboxBackend, SandboxProfile};

    sb.enabled
        && sb.backend == SandboxBackend::Auto
        && sb.profile == SandboxProfile::Open
        && sb.network == Network::Nat
        && sb.network_allow.is_empty()
        && sb.network_block.is_empty()
        && !sb.network_audit
        && !sb.vpn.is_enabled()
        && sb.isolation_floor == thegn_core::config::IsolationFloor::Off
        // The CLI has no equivalent for thegn's remote daemon or OCI runtime
        // selection. Falling through preserves both the user's endpoint and
        // any stronger userspace/guest-kernel isolation they requested.
        && sb.oci_host.trim().is_empty()
        && sb.oci_runtime.trim().is_empty()
        // The provider always bind-mounts the workspace. It cannot honor a
        // request for unrestricted/custom/empty host-file access.
        && matches!(sb.file_access, FileAccess::Worktree | FileAccess::WorktreePlusCaches)
        // Per-pane ceilings are part of the native sandbox contract and are
        // not represented by the devcontainer CLI.
        && sb.limits.cpu.is_none()
        && sb.limits.memory.is_none()
}

/// Opaque handle returned after `devcontainer up` succeeds.
#[derive(Clone)]
pub(crate) struct DevcontainerHandle {
    executable: PathBuf,
    workspace_folder: PathBuf,
    config_path: PathBuf,
    config_digest: [u8; 32],
    /// Keeps the immutable provider config alive for the lifetime of the
    /// session. It is created beside the original config so relative paths in
    /// Dockerfile/Compose/mount fields retain devcontainer semantics.
    config_snapshot: Arc<tempfile::NamedTempFile>,
    /// Values explicitly admitted by `[sandbox].env_passthrough`. The provider
    /// CLI receives these values for `${localEnv:...}` expansion; all other
    /// host variables are deliberately absent from its environment.
    env: Vec<(String, String)>,
}

impl std::fmt::Debug for DevcontainerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevcontainerHandle")
            .field("workspace_folder", &self.workspace_folder)
            .field("config_path", &self.config_path)
            .finish_non_exhaustive()
    }
}

/// Host implementation seam. The trait is object-safe so a future host-side
/// implementation can replace the CLI without changing launch call sites.
pub(crate) trait DevcontainerProvider: Send + Sync {
    fn probe(&self) -> ProbeReport;
    fn start(
        &self,
        workspace_folder: &Path,
        config_path: &Path,
        config_digest: &[u8; 32],
        config_content: &[u8],
        env: &[(String, String)],
    ) -> anyhow::Result<DevcontainerHandle>;
    fn exec_argv(&self, handle: &DevcontainerHandle, command: &str) -> anyhow::Result<Vec<String>>;
}

/// A started provider session. Its `exec_argv` adapter remains provider-owned;
/// launch code only asks the session for the final argv.
#[derive(Clone)]
pub(crate) struct DevcontainerSession {
    provider: Arc<dyn DevcontainerProvider>,
    handle: DevcontainerHandle,
}

impl std::fmt::Debug for DevcontainerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevcontainerSession")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl DevcontainerSession {
    pub(crate) fn start(
        provider: Arc<dyn DevcontainerProvider>,
        workspace_folder: &Path,
        config_path: &Path,
        config_digest: &[u8; 32],
        config_content: &[u8],
        env: &[(String, String)],
    ) -> anyhow::Result<Self> {
        let handle = provider.start(
            workspace_folder,
            config_path,
            config_digest,
            config_content,
            env,
        )?;
        Ok(Self { provider, handle })
    }

    pub(crate) fn exec_argv(&self, command: &str) -> anyhow::Result<Vec<String>> {
        self.provider.exec_argv(&self.handle, command)
    }

    fn verify_config(&self) -> anyhow::Result<()> {
        verify_config_digest(&self.handle.config_path, &self.handle.config_digest)
    }

    /// Environment additions for the host-side provider `exec` command. The
    /// pane spawn chokepoint supplies its own safe runtime base environment;
    /// these are only the explicitly allowlisted local-env values.
    pub(crate) fn exec_env(&self) -> &[(String, String)] {
        &self.handle.env
    }
}

pub(crate) fn provider() -> Arc<dyn DevcontainerProvider> {
    Arc::new(CliProvider::discover())
}

pub(crate) fn probe() -> ProbeReport {
    provider().probe()
}

fn sessions() -> &'static std::sync::Mutex<std::collections::HashMap<String, DevcontainerSession>> {
    static SESSIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, DevcontainerSession>>,
    > = std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn publish_session(worktree: &str, session: DevcontainerSession) {
    sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(worktree.to_string(), session);
}

pub(crate) fn session_for(worktree: &str) -> Option<DevcontainerSession> {
    let session = sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(worktree)
        .cloned()?;
    if let Err(error) = session.verify_config() {
        tracing::warn!(
            target: "thegn::config_trust",
            worktree,
            "devcontainer session invalidated: {error}"
        );
        sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(worktree);
        return None;
    }
    Some(session)
}

/// Derive the single status decision shared by launch, doctor, and hydration.
/// No process is started here; the caller supplies the already-bounded probe.
pub(crate) fn status_for_selected(
    config: &thegn_core::devcontainer::DevContainer,
    selection: &thegn_core::devcontainer_select::SelectionResult,
    worktree: &Path,
    sandbox: &thegn_core::config::SandboxConfig,
    approvals: &thegn_core::config_resolve::Approvals,
    probe: &ProbeReport,
) -> DevcontainerStatus {
    let variant = selection
        .selected
        .as_deref()
        .map(|path| {
            thegn_core::devcontainer_select::relative_path(worktree, path)
                .display()
                .to_string()
        })
        .unwrap_or_default();
    let mut folded = sandbox.clone();
    let allowed = sandbox.env_passthrough.clone();
    let local_env = |key: &str| {
        allowed
            .iter()
            .any(|allowed_key| allowed_key == key)
            .then(|| std::env::var(key).ok())
            .flatten()
    };
    let allow_local_env = |key: &str| allowed.iter().any(|allowed_key| allowed_key == key);
    let ctx = thegn_core::devcontainer::SubstCtx {
        local_workspace_folder: String::new(),
        container_workspace_folder: String::new(),
        local_env: &local_env,
        container_env: &|_| None,
    };
    let outcome = thegn_core::devcontainer_overlay::apply_gated_with_policy(
        config,
        &mut folded,
        &ctx,
        "",
        approvals,
        &allow_local_env,
    );
    let source_present = match &config.source {
        thegn_core::devcontainer::ImageSource::Image(image) => !image.is_empty(),
        thegn_core::devcontainer::ImageSource::Build(_)
        | thegn_core::devcontainer::ImageSource::Compose(_) => true,
    };
    let source_approved = !outcome.pending.iter().any(|request| {
        request.key.starts_with("devcontainer.image")
            || request.key.starts_with("devcontainer.build")
            || request.key.starts_with("devcontainer.compose")
    });
    let inventory = thegn_core::devcontainer::recognized_unapplied(config);
    let user_source_pinned =
        !sandbox.image.is_empty() || sandbox.build.is_some() || sandbox.compose.is_some();
    let provider_eligible = source_present
        && can_honor_sandbox(&folded)
        && !user_source_pinned
        && source_approved
        && outcome.substitution.blocked_local_env.is_empty()
        && inventory.refused.is_empty()
        && inventory.reserved.is_empty()
        && inventory.unknown.is_empty();
    let state = if !source_present {
        DevcontainerState::Degraded
    } else if !source_approved || !outcome.pending.is_empty() {
        DevcontainerState::Pending
    } else if probe.ready() && provider_eligible {
        DevcontainerState::Ready
    } else {
        DevcontainerState::Degraded
    };
    let reason = if !source_present {
        Some("no image/build/compose source".into())
    } else if !source_approved {
        Some("container source awaits trust approval".into())
    } else if !outcome.pending.is_empty() {
        Some("devcontainer requests await trust approval".into())
    } else if !provider_eligible {
        Some("config contains fields the CLI provider cannot safely apply".into())
    } else {
        probe.reason.clone()
    };
    DevcontainerStatus {
        variant,
        state,
        reason,
    }
}

/// Discover and classify the repo config for read-only surfaces. Selection
/// errors are status too: users should be able to see why a variant did not
/// become active without starting a provider or applying repo-authored data.
pub(crate) fn status_for_worktree(
    cfg: &thegn_core::config::Config,
    repo_root: &Path,
    worktree: &Path,
    sandbox: &thegn_core::config::SandboxConfig,
    approvals: &thegn_core::config_resolve::Approvals,
    probe: &ProbeReport,
) -> Option<DevcontainerStatus> {
    if sandbox.devcontainer == thegn_core::config::DevcontainerMode::Off {
        return Some(DevcontainerStatus {
            variant: String::new(),
            state: DevcontainerState::Off,
            reason: Some("disabled by [sandbox] devcontainer = off".into()),
        });
    }
    let selection = thegn_core::devcontainer_select::select_and_parse(
        worktree,
        Some(&cfg.repo_devcontainer_selector(repo_root)),
    );
    if selection.candidates.is_empty() {
        return None;
    }
    let Some(config) = selection.config.as_ref() else {
        let (state, reason) = match selection.error.as_ref() {
            Some(thegn_core::devcontainer_select::SelectionError::Ambiguous(_)) => (
                DevcontainerState::Ambiguous,
                "multiple devcontainer variants require a selector".to_string(),
            ),
            Some(error) => (DevcontainerState::Invalid, error.to_string()),
            None => (
                DevcontainerState::Invalid,
                "devcontainer config unavailable".into(),
            ),
        };
        return Some(DevcontainerStatus {
            variant: String::new(),
            state,
            reason: Some(reason),
        });
    };
    Some(status_for_selected(
        config, &selection, worktree, sandbox, approvals, probe,
    ))
}

struct CliProvider {
    executable: Option<PathBuf>,
}

impl CliProvider {
    fn discover() -> Self {
        Self {
            executable: thegn_core::util::which_path(CLI_NAME).map(PathBuf::from),
        }
    }

    #[cfg(test)]
    fn with_executable(path: impl Into<PathBuf>) -> Self {
        Self {
            executable: Some(path.into()),
        }
    }

    fn executable(&self) -> anyhow::Result<&Path> {
        self.executable
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("`{CLI_NAME}` not found on PATH"))
    }
}

impl DevcontainerProvider for CliProvider {
    fn probe(&self) -> ProbeReport {
        let Some(executable) = self.executable.as_deref() else {
            return ProbeReport::unavailable(format!("`{CLI_NAME}` not found on PATH"));
        };
        let mut command = Command::new(executable);
        command
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match run_bounded(&mut command, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => ProbeReport {
                state: ProbeState::Ready,
                executable: Some(executable.display().to_string()),
                version: first_line(&output.stdout).or_else(|| first_line(&output.stderr)),
                reason: None,
            },
            Ok(output) => ProbeReport {
                state: ProbeState::Degraded,
                executable: Some(executable.display().to_string()),
                version: first_line(&output.stdout).or_else(|| first_line(&output.stderr)),
                reason: Some(format!(
                    "`{CLI_NAME} --version` exited with {}",
                    output.status
                )),
            },
            Err(error) => ProbeReport {
                state: ProbeState::Degraded,
                executable: Some(executable.display().to_string()),
                version: None,
                reason: Some(format!("bounded version probe failed: {error}")),
            },
        }
    }

    fn start(
        &self,
        workspace_folder: &Path,
        config_path: &Path,
        config_digest: &[u8; 32],
        config_content: &[u8],
        env: &[(String, String)],
    ) -> anyhow::Result<DevcontainerHandle> {
        let executable = self.executable()?.to_path_buf();
        let snapshot = snapshot_config(config_path, config_digest, config_content)?;
        let provider_config_path = snapshot.path().to_path_buf();
        let mut command = Command::new(&executable);
        command
            .args(["up", "--workspace-folder"])
            .arg(workspace_folder)
            .args(["--config"])
            .arg(&provider_config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // The CLI reads the raw repository config and may evaluate
        // `${localEnv:NAME}` itself. Clear the inherited launcher environment
        // before restoring only safe runtime plumbing and the effective
        // sandbox allowlist, so vendor parsing cannot read an arbitrary host
        // secret.
        let allowlisted_env = env.to_vec();
        let provider_env = provider_env(env);
        command.env_clear();
        for (key, value) in &provider_env {
            command.env(key, value);
        }
        // Keep the final check adjacent to the child spawn. The earlier check
        // rejects a stale path before any provider setup; this one closes the
        // setup window immediately before the CLI reads the config.
        // The child receives the immutable snapshot, so a repository edit
        // after this check cannot change what the provider parses. Keep the
        // check for the expected stale-session behavior.
        verify_config_digest(config_path, config_digest)?;
        let output = run_bounded(&mut command, START_TIMEOUT)?;
        anyhow::ensure!(
            output.status.success(),
            "`{CLI_NAME} up` failed with {}{}",
            output.status,
            stderr_suffix(&output.stderr)
        );
        Ok(DevcontainerHandle {
            executable,
            workspace_folder: workspace_folder.to_path_buf(),
            config_path: config_path.to_path_buf(),
            config_digest: *config_digest,
            config_snapshot: snapshot,
            env: allowlisted_env,
        })
    }

    fn exec_argv(&self, handle: &DevcontainerHandle, command: &str) -> anyhow::Result<Vec<String>> {
        verify_config_digest(&handle.config_path, &handle.config_digest)?;
        // The CLI accepts the command after the workspace/config options. Keep
        // it as one shell command so the core's normalized command semantics
        // and init hooks remain unchanged.
        Ok(vec![
            handle.executable.display().to_string(),
            "exec".into(),
            "--workspace-folder".into(),
            handle.workspace_folder.display().to_string(),
            "--config".into(),
            handle.config_snapshot.path().display().to_string(),
            "sh".into(),
            "-lc".into(),
            command.into(),
        ])
    }
}

/// Hash the selected file's bytes for the provider trust boundary. The
/// provider snapshot is kept beside the original so the devcontainer CLI
/// resolves Dockerfile, Compose, and mount paths relative to the same
/// directory.
pub(crate) fn config_digest(path: &Path) -> anyhow::Result<[u8; 32]> {
    let bytes = std::fs::read(path)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
    Ok(config_digest_bytes(&bytes))
}

fn config_digest_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn snapshot_config(
    path: &Path,
    expected: &[u8; 32],
    content: &[u8],
) -> anyhow::Result<Arc<tempfile::NamedTempFile>> {
    anyhow::ensure!(
        config_digest_bytes(content) == *expected,
        "parsed devcontainer config content no longer matches its trust digest"
    );
    // Reject an already-observed repository change, but never pass this
    // mutable path to the provider. A later replacement is harmless because
    // the child only receives the snapshot below.
    verify_config_digest(path, expected)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::Builder::new()
        .prefix(".thegn-devcontainer-")
        .suffix(".json")
        .tempfile_in(parent)?;
    file.write_all(content)?;
    file.flush()?;
    Ok(Arc::new(file))
}

fn verify_config_digest(path: &Path, expected: &[u8; 32]) -> anyhow::Result<()> {
    let actual = config_digest(path)?;
    anyhow::ensure!(
        actual == *expected,
        "devcontainer config changed after trust approval; refusing provider use"
    );
    Ok(())
}

fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

fn stderr_suffix(bytes: &[u8]) -> String {
    first_line(bytes)
        .map(|line| format!(": {line}"))
        .unwrap_or_default()
}

/// Build the provider process environment from the same safe runtime base as
/// pane processes, then append the effective local-env allowlist. The latter
/// wins if a caller explicitly admits an infrastructure key with a different
/// value.
fn provider_env(allowlisted: &[(String, String)]) -> Vec<(String, String)> {
    // Do not use `host_base_env` here: its optional process-wide extras are
    // intended for ordinary panes, while raw devcontainer substitution is
    // constrained specifically by this sandbox's `env_passthrough` list.
    let mut env = thegn_core::util::filter_host_env(std::env::vars(), &[]);
    env.extend(allowlisted.iter().cloned());
    env
}

#[expect(clippy::disallowed_methods)]
fn run_bounded(command: &mut Command, timeout: Duration) -> anyhow::Result<std::process::Output> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                std::io::Read::read_to_end(&mut pipe, &mut stdout)?;
            }
            if let Some(mut pipe) = child.stderr.take() {
                std::io::Read::read_to_end(&mut pipe, &mut stderr)?;
            }
            return Ok(std::process::Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("process exceeded {:?} timeout", timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_exec_argv_keeps_provider_flags_inside_the_seam_for_verified_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("devcontainer.json");
        let content = b"{\"image\":\"repo\"}";
        std::fs::write(&config_path, content).unwrap();
        let digest = config_digest(&config_path).unwrap();
        let snapshot = snapshot_config(&config_path, &digest, content).unwrap();
        let provider = CliProvider::with_executable("/bin/devcontainer");
        let handle = DevcontainerHandle {
            executable: "/bin/devcontainer".into(),
            workspace_folder: "/repo/worktree".into(),
            config_path: config_path.clone(),
            config_digest: digest,
            config_snapshot: snapshot,
            env: Vec::new(),
        };
        assert_eq!(
            provider.exec_argv(&handle, "printf ok").unwrap(),
            vec![
                "/bin/devcontainer".into(),
                "exec".into(),
                "--workspace-folder".into(),
                "/repo/worktree".into(),
                "--config".into(),
                handle.config_snapshot.path().display().to_string(),
                "sh".into(),
                "-lc".into(),
                "printf ok".into(),
            ]
        );
    }

    #[test]
    fn provider_rejects_config_changed_after_trust() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("devcontainer.json");
        let content = b"{\"image\":\"approved\"}";
        std::fs::write(&config_path, content).unwrap();
        let digest = config_digest(&config_path).unwrap();
        std::fs::write(&config_path, "{\"image\":\"changed\"}").unwrap();

        let provider = CliProvider::with_executable("/bin/devcontainer");
        let error = provider
            .start(dir.path(), &config_path, &digest, content, &[])
            .expect_err("changed config must not reach the provider");
        assert!(error.to_string().contains("changed after trust approval"));
    }

    #[test]
    fn provider_rejects_config_changed_before_exec() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("devcontainer.json");
        let content = b"{\"image\":\"approved\"}";
        std::fs::write(&config_path, content).unwrap();
        let digest = config_digest(&config_path).unwrap();
        let snapshot = snapshot_config(&config_path, &digest, content).unwrap();
        let handle = DevcontainerHandle {
            executable: "/bin/devcontainer".into(),
            workspace_folder: dir.path().into(),
            config_path: config_path.clone(),
            config_digest: digest,
            config_snapshot: snapshot,
            env: Vec::new(),
        };
        std::fs::write(&config_path, "{\"image\":\"changed\"}").unwrap();

        let error = CliProvider::with_executable("/bin/devcontainer")
            .exec_argv(&handle, "printf ok")
            .expect_err("changed config must not produce an exec command");
        assert!(error.to_string().contains("changed after trust approval"));
    }

    #[cfg(unix)]
    #[test]
    fn provider_up_reads_the_approved_snapshot_after_original_changes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("provider-config");
        let script = dir.path().join("devcontainer");
        let config_path = dir.path().join("devcontainer.json");
        let approved = b"{\"image\":\"approved\"}";
        std::fs::write(&config_path, approved).unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '{{\"image\":\"changed\"}}' > '{}'\ncat \"$5\" > '{}'\n",
                config_path.display(),
                report.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let digest = config_digest(&config_path).unwrap();

        let handle = CliProvider::with_executable(script)
            .start(dir.path(), &config_path, &digest, approved, &[])
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(report).unwrap(),
            String::from_utf8_lossy(approved)
        );
        assert!(handle.config_snapshot.path().exists());
    }

    #[test]
    fn provider_rejects_unrepresentable_sandbox_policy() {
        let mut sb = thegn_core::config::SandboxConfig::default();
        assert!(!can_honor_sandbox(&sb));

        sb.profile = thegn_core::config::SandboxProfile::Open;
        assert!(can_honor_sandbox(&sb));

        sb.network = thegn_core::config::Network::None;
        assert!(!can_honor_sandbox(&sb));
        sb.network = thegn_core::config::Network::Nat;
        sb.enabled = false;
        assert!(!can_honor_sandbox(&sb));
        sb.enabled = true;
        sb.oci_host = "ssh://builder".into();
        assert!(!can_honor_sandbox(&sb));
        sb.oci_host.clear();
        sb.oci_runtime = "runsc".into();
        assert!(!can_honor_sandbox(&sb));
        sb.oci_runtime.clear();
        sb.network_audit = true;
        assert!(!can_honor_sandbox(&sb));
    }

    #[test]
    fn provider_env_contains_only_runtime_and_explicit_values() {
        let env = provider_env(&[("DC_ALLOWED".into(), "yes".into())]);
        assert_eq!(
            env.iter()
                .find(|(key, _)| key == "DC_ALLOWED")
                .map(|(_, value)| value.as_str()),
            Some("yes")
        );
        assert!(!env.iter().any(|(key, _)| key == "DC_NOT_ALLOWLISTED"));
    }

    #[cfg(unix)]
    #[test]
    fn provider_up_cannot_observe_a_non_allowlisted_host_variable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("environment");
        let script = dir.path().join("devcontainer");
        std::fs::write(
            &script,
            format!("#!/bin/sh\n/usr/bin/env > '{}'\n", report.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config_path = dir.path().join("devcontainer.json");
        let content = b"{\"image\":\"repo\"}";
        std::fs::write(&config_path, content).unwrap();
        let config_digest = config_digest(&config_path).unwrap();

        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let blocked = "DC_PROVIDER_BLOCKED";
        let previous = std::env::var_os(blocked);
        // SAFETY: the test serializes its process-environment mutation and
        // restores the prior value before releasing the lock.
        unsafe { std::env::set_var(blocked, "must-not-cross") };
        let provider = CliProvider::with_executable(&script);
        let handle = provider
            .start(
                dir.path(),
                &config_path,
                &config_digest,
                content,
                &[("DC_PROVIDER_ALLOWED".into(), "yes".into())],
            )
            .unwrap();
        // SAFETY: paired with the serialized setup above.
        match previous {
            Some(value) => unsafe { std::env::set_var(blocked, value) },
            None => unsafe { std::env::remove_var(blocked) },
        }

        let body = std::fs::read_to_string(report).unwrap();
        assert!(body.lines().any(|line| line == "DC_PROVIDER_ALLOWED=yes"));
        assert!(
            !body
                .lines()
                .any(|line| line == "DC_PROVIDER_BLOCKED=must-not-cross")
        );
        assert_eq!(
            handle.env,
            vec![("DC_PROVIDER_ALLOWED".into(), "yes".into())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn provider_up_failure_preserves_stderr_for_diagnostics() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("devcontainer");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'provider broke\\n' >&2\nexit 23\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config_path = dir.path().join("devcontainer.json");
        let content = b"{\"image\":\"repo\"}";
        std::fs::write(&config_path, content).unwrap();
        let config_digest = config_digest(&config_path).unwrap();

        let error = CliProvider::with_executable(script)
            .start(dir.path(), &config_path, &config_digest, content, &[])
            .expect_err("failed provider start must be reported");
        let message = error.to_string();
        assert!(message.contains("failed with"), "{message}");
        assert!(message.contains("provider broke"), "{message}");
    }

    #[test]
    fn status_does_not_claim_provider_ready_when_user_pinned_source_wins() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("devcontainer.json");
        std::fs::write(&config_path, r#"{"image":"repo-image"}"#).unwrap();
        let selection = thegn_core::devcontainer_select::select_and_parse(dir.path(), None);
        let config = selection.config.as_ref().unwrap();
        let approvals = thegn_core::config_resolve::Approvals::from_canonical(
            thegn_core::devcontainer_overlay::gate_requests(config)
                .into_iter()
                .map(|request| request.canonical()),
        );
        let mut sandbox = thegn_core::config::SandboxConfig::default();
        sandbox.profile = thegn_core::config::SandboxProfile::Open;
        sandbox.image = "trusted-image".into();
        let status = status_for_selected(
            config,
            &selection,
            dir.path(),
            &sandbox,
            &approvals,
            &ProbeReport {
                state: ProbeState::Ready,
                executable: Some("devcontainer".into()),
                version: Some("1".into()),
                reason: None,
            },
        );
        assert_eq!(status.state, DevcontainerState::Degraded);
    }
}
