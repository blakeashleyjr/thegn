//! Cached, presence-only worktree toolchain status for panels and chips.

use std::path::Path;

use thegn_core::config::Config;

pub(crate) use crate::mise_provider::ToolchainStatus;

pub(crate) fn worktree_status(
    cfg: &Config,
    worktree: &Path,
    repo_root: &Path,
    db: Option<&thegn_core::db::Db>,
) -> ToolchainStatus {
    crate::mise_provider::status(cfg, worktree, repo_root, db)
}
