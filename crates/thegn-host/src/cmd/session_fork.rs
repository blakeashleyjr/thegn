//! CLI composition for session forks.
//!
//! The daemon remains the only spawn owner. This module handles the optional
//! worktree-create step and formats the resulting control response.

use anyhow::{Context, Result, bail};
use std::path::{Component, Path, PathBuf};

use thegn_core::outln;
use thegn_svc::control::client::ControlClient;
use thegn_svc::control::{ForkSpec, WorktreeCreateReq};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    client: &ControlClient,
    session: String,
    harness: Option<String>,
    agent: Option<String>,
    cwd: Option<String>,
    worktree: Option<String>,
    scrollback: bool,
    fork_worktree: bool,
    tab: bool,
    json: bool,
) -> Result<()> {
    let mut final_cwd = cwd;
    let mut final_worktree = worktree;
    let mut created_worktree: Option<String> = None;

    if fork_worktree {
        if harness.is_some() {
            bail!("--fork-worktree applies only to a live daemon session");
        }
        let sessions = client.sessions().await?;
        let source = sessions
            .iter()
            .find(|info| info.id == session)
            .context("source session not found")?;
        if source.exited_at_ms.is_some() {
            bail!("source session has exited; use sessions.open for a cold start");
        }
        let source_wt = source
            .worktree
            .as_deref()
            .filter(|path| !path.is_empty())
            .context("source session has no worktree for --fork-worktree")?;
        let branch = format!("fork/{session}");
        let created = client
            .worktree_create(&WorktreeCreateReq {
                repo: Some(source_wt.to_string()),
                issue: None,
                branch: Some(branch),
            })
            .await
            .context("create fork worktree")?;
        let new_root = PathBuf::from(&created.path);
        let old_root = Path::new(source_wt);
        final_cwd = Some(match final_cwd {
            Some(path) => remap_cwd(old_root, &new_root, Path::new(&path)),
            None => created.path.clone(),
        });
        final_worktree = Some(created.path.clone());
        created_worktree = Some(created.path);
    }

    let info = match client
        .fork(&ForkSpec {
            session,
            harness,
            agent,
            cwd: final_cwd,
            worktree: final_worktree,
            scrollback,
            adopt: true,
            tab,
        })
        .await
    {
        Ok(info) => info,
        Err(error) => {
            if let Some(path) = created_worktree {
                bail!("{error}; fork worktree survives at {path}");
            }
            return Err(error);
        }
    };
    if json {
        // Through the chokepoint, which also makes this COMPACT like every
        // other `--json` in the CLI. It was the lone `to_string_pretty`, so a
        // consumer parsing the fleet of verbs got one differently-shaped
        // document from this one.
        super::emit_json(&info)?;
    } else {
        let lineage = info
            .forked_from
            .as_deref()
            .map(|source| format!(" (forked from {source})"))
            .unwrap_or_default();
        outln!("{}{}", info.id, lineage);
    }
    Ok(())
}

fn remap_cwd(old_root: &Path, new_root: &Path, cwd: &Path) -> String {
    cwd.strip_prefix(old_root)
        .ok()
        .filter(|relative| {
            !relative
                .components()
                .any(|component| component == Component::ParentDir)
        })
        .map(|relative| new_root.join(relative))
        .unwrap_or_else(|| new_root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::remap_cwd;
    use std::path::Path;

    #[test]
    fn remaps_a_cwd_inside_the_source_worktree() {
        assert_eq!(
            remap_cwd(Path::new("/old"), Path::new("/new"), Path::new("/old/src")),
            "/new/src"
        );
    }

    #[test]
    fn outside_cwd_falls_back_to_the_new_worktree_root() {
        assert_eq!(
            remap_cwd(
                Path::new("/old"),
                Path::new("/new"),
                Path::new("/elsewhere")
            ),
            "/new"
        );
    }

    #[test]
    fn cwd_with_parent_traversal_falls_back_to_the_new_worktree_root() {
        assert_eq!(
            remap_cwd(
                Path::new("/old"),
                Path::new("/new"),
                Path::new("/old/../outside")
            ),
            "/new"
        );
    }
}
