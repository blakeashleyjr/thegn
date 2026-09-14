//! Read-only discovery identities and explicitly admitted local snapshots (THE-595).
//!
//! Git/SQLite are not one transaction. Revalidation refuses observed reassignment;
//! it is not a lease against an external actor concurrently rewriting Git metadata.

use super::{Branch, Candidates};
use anyhow::{Context, Result, ensure};
use std::io::Read;
use std::path::{Path, PathBuf};
use thegn_core::config::MergeQueueConfig;
use thegn_core::remote::GitLoc;
use thegn_core::util;
use thegn_svc::git::{CliGit, GitBackend, PlumbingOps};

#[cfg(test)]
#[path = "integrate_candidate_tests.rs"]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalIdentity {
    worktree: PathBuf,
    repo: PathBuf,
    common: PathBuf,
    git_dir: PathBuf,
    branch: String,
    pub(crate) tip: String,
}

fn git_path(path: &Path, flag: &str) -> Result<PathBuf> {
    let value = util::git_out(path, &["rev-parse", "--path-format=absolute", flag])
        .with_context(|| format!("cannot inspect local Git {flag}"))?;
    std::fs::canonicalize(value.trim()).context("cannot canonicalize local Git identity")
}

impl LocalIdentity {
    pub(crate) fn read(repo: &Path, worktree: &Path, branch: &str) -> Result<Self> {
        ensure!(
            thegn_core::sandbox_backend::host_os() != thegn_core::sandbox_backend::HostOs::Windows,
            "strict local snapshot admission is unsupported on Windows; disable snapshot_dirty and commit explicitly"
        );
        let repo = std::fs::canonicalize(repo).context("local repository unavailable")?;
        let canonical = std::fs::canonicalize(worktree).context("local worktree unavailable")?;
        ensure!(
            worktree.as_os_str() == canonical.as_os_str(),
            "noncanonical or aliased worktree path refused"
        );
        let worktree = canonical;
        ensure!(repo != worktree, "snapshot cannot target the main checkout");
        ensure!(
            git_path(&worktree, "--show-toplevel")? == worktree,
            "worktree root changed"
        );
        let common = git_path(&repo, "--git-common-dir")?;
        ensure!(
            git_path(&worktree, "--git-common-dir")? == common,
            "worktree repository changed"
        );
        let git_dir = git_path(&worktree, "--absolute-git-dir")?;
        ensure!(
            git_dir.parent() == Some(common.join("worktrees").as_path()),
            "not a linked Git worktree"
        );
        let loc = GitLoc::Local(worktree.clone());
        ensure!(
            CliGit.current_branch(&loc)? == branch,
            "candidate branch changed"
        );
        let tip = CliGit.rev_parse(&loc, "HEAD")?;
        // The per-worktree administration directory must point back to this exact
        // checkout, not merely live under another repository's worktrees folder.
        let backlink = read_backlink(&git_dir.join("gitdir"))?;
        let backlink = std::fs::canonicalize(backlink.trim())
            .context("linked worktree registration target unavailable")?;
        ensure!(
            backlink == std::fs::canonicalize(worktree.join(".git"))?,
            "linked worktree registration changed"
        );
        Ok(Self {
            worktree,
            repo,
            common,
            git_dir,
            branch: branch.to_owned(),
            tip,
        })
    }

    pub(crate) fn loc(&self) -> GitLoc {
        // Never call for_worktree/from_db here: an untrusted/reassigned registry
        // location must not retarget the already selected operation remotely.
        GitLoc::Local(self.worktree.clone())
    }

    fn revalidate(&self) -> Result<()> {
        ensure!(
            Self::read(&self.repo, &self.worktree, &self.branch)? == *self,
            "candidate Git identity changed since discovery"
        );
        Ok(())
    }
}

fn read_backlink(path: &Path) -> Result<String> {
    const LIMIT: u64 = 64 * 1024;
    let file =
        crate::platform::open_nofollow(path).context("linked worktree registration unavailable")?;
    let meta = file
        .metadata()
        .context("registration metadata unavailable")?;
    ensure!(
        meta.is_file() && meta.len() <= LIMIT,
        "registration is not a bounded regular file"
    );
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .context("registration read failed")?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "registration exceeded size limit"
    );
    String::from_utf8(bytes).context("registration path is not UTF-8")
}

/// Called only after selection/confirmation and the THE-591 pre-fold observation.
/// All selected identities are checked before the first snapshot. Only their tips
/// are replaced; unselected paths in `Candidates.worktrees` never confer authority.
pub(super) fn selected_snapshot_tips(
    config: &MergeQueueConfig,
    repo_root: &Path,
    candidates: &Candidates,
    observations: Option<&super::persistence::OutcomeObservations>,
    override_gpg: bool,
    history: &crate::canonical_history::CanonicalHistory,
) -> Result<Vec<Branch>> {
    history.revalidate()?;
    if !config.snapshot_dirty || candidates.branches.is_empty() {
        return Ok(candidates.branches.clone());
    }
    let observations =
        observations.context("snapshot admission requires a pre-fold database observation")?;
    let repo = std::fs::canonicalize(repo_root)?;
    let mut selected = Vec::new();
    for branch in &candidates.branches {
        let identity = candidates
            .identities
            .get(&branch.name)
            .context("snapshot candidate has no discovered local identity")?;
        ensure!(
            identity.repo == repo && identity.tip == branch.tip,
            "snapshot candidate changed"
        );
        let path = candidates
            .worktrees
            .get(&branch.name)
            .context("snapshot candidate path missing")?;
        ensure!(
            Path::new(path) == identity.worktree,
            "snapshot candidate path changed"
        );
        if let Some(row) = observations.registry_identity(path)? {
            ensure!(
                row.location
                    .as_deref()
                    .is_none_or(|v| v.is_empty() || v == "local"),
                "snapshot refused for nonlocal or unsupported registry placement"
            );
            ensure!(
                row.branch.as_deref() == Some(branch.name.as_str())
                    && row.repo_path.as_deref() == repo.to_str(),
                "snapshot registry identity changed"
            );
        }
        identity.revalidate()?;
        let source = history.local_child(&identity.loc())?;
        CliGit
            .is_dirty(&identity.loc())
            .context("selected dirty-state lookup failed")?;
        selected.push((identity, source));
    }
    let mut tips = Vec::with_capacity(selected.len());
    let mut completed = 0usize;
    for (identity, source) in selected {
        let snapshot = (|| {
            history.revalidate()?;
            identity.revalidate()?;
            let result = source.checked(|| CliGit.snapshot_worktree(&identity.loc(), &format!("snapshot: {} (fold-actor)", identity.branch), override_gpg));
            history.revalidate()?;
            result
        })().with_context(|| format!(
            "snapshot of selected branch {} failed; {completed} earlier authorized snapshots may remain; this worktree may be staged; no rollback was attempted",
            identity.branch.chars().filter(|c| !c.is_control() && super::diagnostic_char(*c)).take(160).collect::<String>()
        ))?;
        if snapshot.is_some() {
            completed += 1;
        }
        tips.push(Branch {
            name: identity.branch.clone(),
            tip: snapshot.unwrap_or_else(|| identity.tip.clone()),
        });
    }
    Ok(tips)
}
