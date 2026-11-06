//! Shared selection and presentation helpers for worktree isolation profiles.

use anyhow::Result;
use std::path::Path;
use thegn_core::sandbox_pin::EnvSelection;
use thegn_core::store::WorkspaceStore;

pub(crate) fn selection_for(worktree: &str, repo_root: &Path) -> Result<EnvSelection> {
    let db = thegn_core::db::Db::open()?;
    db.effective_env_selection(worktree, &repo_root.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::selection_for;
    use thegn_core::config::SandboxProfile;
    use thegn_core::store::WorkspaceStore;

    #[test]
    fn sandbox_profile_selection_for_reads_the_stored_pin() {
        let _state = crate::testenv::IsolatedState::new();
        let root = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open().unwrap();
        db.set_worktree_sandbox_profile(
            root.path().to_string_lossy().as_ref(),
            Some(SandboxProfile::Sealed),
        )
        .unwrap();
        let selection = selection_for(root.path().to_string_lossy().as_ref(), root.path()).unwrap();
        assert_eq!(selection.profile, Some(SandboxProfile::Sealed));
    }
}
