//! `superzej worktrees` — (internal) TSV of every managed on-disk worktree for
//! the sidebar plugin: `repo_slug<TAB>branch_label<TAB>worktree_path` per line.
//!
//! Mirrors `workspaces` (which lists repos). The sidebar merges this with the
//! live `TabUpdate` so worktrees that exist on disk but have no open tab still
//! appear — and selecting one opens it (`superzej open-worktree`). `branch_label`
//! is the slugified branch, matching the `{slug}/{slugify(branch)}` tab name so
//! the two feeds key identically.

use crate::config::Config;
use crate::db::Db;
use crate::{commands, repo, util};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

pub fn run(cfg: &Config) -> Result<()> {
    // Reuse one DB connection and cache the slug per repo — worktrees share
    // repos, so the old per-entry `repo::repo_slug` reopened the DB and spawned
    // `git` once *per worktree* (hundreds of times). Same cheap path as the
    // `workspaces` command.
    let db = Db::open()?;
    let mut slugs: HashMap<String, String> = HashMap::new();
    for v in commands::list::collect(cfg)? {
        if !v.exists {
            continue;
        }
        let slug = slugs.entry(v.repo.clone()).or_insert_with(|| {
            let base = {
                let s = util::slugify(&repo::repo_name_from_path(Path::new(&v.repo)));
                if s.is_empty() { "repo".to_string() } else { s }
            };
            db.slug_for_repo(&v.repo, &base).unwrap_or(base)
        });
        let label = util::slugify(&v.branch);
        crate::outln!("{slug}\t{label}\t{}", v.path);
    }
    Ok(())
}
