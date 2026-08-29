//! Host-owned provider seam for the optional `devcontainer` CLI.
//!
//! The core only discovers, parses, classifies, and trust-gates a
//! `devcontainer.json`. This module owns the process boundary: executable
//! discovery, bounded version probing, `up`, and `exec` argv construction.
//! Callers receive an opaque session and never need to know CLI-specific flags.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Opaque handle returned after `devcontainer up` succeeds.
#[derive(Clone)]
pub(crate) struct DevcontainerHandle {
    executable: PathBuf,
    workspace_folder: PathBuf,
    config_path: PathBuf,
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
    ) -> anyhow::Result<DevcontainerHandle>;
    fn exec_argv(&self, handle: &DevcontainerHandle, command: &str) -> Vec<String>;
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
    ) -> anyhow::Result<Self> {
        let handle = provider.start(workspace_folder, config_path)?;
        Ok(Self { provider, handle })
    }

    pub(crate) fn exec_argv(&self, command: &str) -> Vec<String> {
        self.provider.exec_argv(&self.handle, command)
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
    sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(worktree)
        .cloned()
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
    let provider_eligible = source_present
        && source_approved
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
    ) -> anyhow::Result<DevcontainerHandle> {
        let executable = self.executable()?.to_path_buf();
        let mut command = Command::new(&executable);
        command
            .args(["up", "--workspace-folder"])
            .arg(workspace_folder)
            .args(["--config"])
            .arg(config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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
        })
    }

    fn exec_argv(&self, handle: &DevcontainerHandle, command: &str) -> Vec<String> {
        // The CLI accepts the command after the workspace/config options. Keep
        // it as one shell command so the core's normalized command semantics
        // and init hooks remain unchanged.
        vec![
            handle.executable.display().to_string(),
            "exec".into(),
            "--workspace-folder".into(),
            handle.workspace_folder.display().to_string(),
            "--config".into(),
            handle.config_path.display().to_string(),
            "sh".into(),
            "-lc".into(),
            command.into(),
        ]
    }
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
    fn cli_exec_argv_keeps_provider_flags_inside_the_seam() {
        let provider = CliProvider::with_executable("/bin/devcontainer");
        let handle = DevcontainerHandle {
            executable: "/bin/devcontainer".into(),
            workspace_folder: "/repo/worktree".into(),
            config_path: "/repo/.devcontainer/devcontainer.json".into(),
        };
        assert_eq!(
            provider.exec_argv(&handle, "printf ok"),
            vec![
                "/bin/devcontainer",
                "exec",
                "--workspace-folder",
                "/repo/worktree",
                "--config",
                "/repo/.devcontainer/devcontainer.json",
                "sh",
                "-lc",
                "printf ok",
            ]
        );
    }
}
