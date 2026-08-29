//! Daemon-side helpers for the sessions.fork operation.
//!
//! The service owns the orchestration and PTY spawn; this module keeps the
//! filesystem handoff and live-source conversion small and auditable. Recipes
//! never leave the live session table.

use std::path::PathBuf;

use anyhow::{Context, Result};
use thegn_core::session_fork::{DaemonRecipe, ForkSource};

use super::service::SessionEntry;

/// Convert a live entry into the pure core source. The entry may be a test or
/// legacy stub without a retained recipe; such a session cannot honestly be
/// forked.
pub(crate) fn source(entry: &SessionEntry) -> Option<ForkSource> {
    Some(ForkSource::DaemonSession {
        id: entry.meta.id.clone(),
        recipe: entry.recipe.clone()?,
    })
}

/// Owner-only handoff path for an optional bounded scrollback context.
pub(crate) fn handoff_path(child_id: &str) -> PathBuf {
    thegn_core::util::xdg_state_home()
        .join("thegn")
        .join("forks")
        .join(format!("{child_id}.txt"))
}

/// Write a plain-text handoff before the child is spawned.
pub(crate) fn write_handoff(child_id: &str, text: &str) -> Result<PathBuf> {
    let path = handoff_path(child_id);
    let dir = path.parent().context("fork handoff has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    thegn_core::fsperm::restrict_dir_to_owner(dir)
        .with_context(|| format!("restrict {}", dir.display()))?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    thegn_core::fsperm::restrict_to_owner(&path)
        .with_context(|| format!("restrict {}", path.display()))?;
    Ok(path)
}

/// Best-effort lifecycle cleanup when the child exits.
pub(crate) fn cleanup_handoff(path: Option<&std::path::Path>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path); // best-effort: fork context is disposable after child exit
    }
}

/// Return the harness recipe for a normal configured-agent open.
pub(crate) fn agent_recipe(
    cfg: &thegn_core::config::Config,
    launch: &thegn_svc::control::AgentLaunch,
    spec: &thegn_svc::control::OpenSpec,
) -> Option<DaemonRecipe> {
    let harness = super::agent_open::harness_for_agent(cfg, &launch.agent)?;
    Some(DaemonRecipe::Agent {
        harness: harness.id().to_string(),
        native_session_id: launch.native_session_id.clone(),
        agent: Some(launch.agent.clone()),
        cwd: spec.cwd.clone(),
        worktree: spec.worktree.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{cleanup_handoff, handoff_path};

    #[test]
    fn handoff_path_is_scoped_to_the_forks_directory() {
        let path = handoff_path("child");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("child.txt")
        );
        assert_eq!(
            path.parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str()),
            Some("forks")
        );
    }

    #[test]
    fn cleanup_handoff_removes_a_disposable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("handoff.txt");
        std::fs::write(&path, "context").expect("write handoff");
        cleanup_handoff(Some(&path));
        assert!(!path.exists());
    }
}
