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
            .stderr(Stdio::piped());
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
