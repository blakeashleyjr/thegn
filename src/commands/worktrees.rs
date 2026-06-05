//! `superzej worktrees` — (internal) TSV of every managed on-disk worktree for
//! the sidebar plugin: `repo_slug<TAB>branch_label<TAB>worktree_path` per line.
//!
//! Mirrors `workspaces` (which lists repos). The sidebar merges this with the
//! live `TabUpdate` so worktrees that exist on disk but have no open tab still
//! appear — and selecting one opens it (`superzej open-worktree`). `branch_label`
//! is the slugified branch, matching the `{slug}/{slugify(branch)}` tab name so
//! the two feeds key identically.

use crate::config::Config;
use crate::{commands, repo, util};
use anyhow::Result;
use std::path::Path;

pub fn run(cfg: &Config) -> Result<()> {
    for v in commands::list::collect(cfg)? {
        if !v.exists {
            continue;
        }
        let slug = repo::repo_slug(Path::new(&v.repo));
        let label = util::slugify(&v.branch);
        println!("{slug}\t{label}\t{}", v.path);
    }
    Ok(())
}
