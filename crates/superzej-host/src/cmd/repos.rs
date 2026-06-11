//! `superzej repos` / `superzej recent` — repo discovery + history feeds.

use anyhow::Result;
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::{outln, repo};

/// Git repos discovered under `repo_roots` (what the picker offers).
pub fn repos(cfg: &Config) -> Result<()> {
    for path in repo::discover_repos(cfg) {
        outln!("{path}");
    }
    Ok(())
}

/// Recently opened repos, most-recent first.
pub fn recent(count: Option<i64>) -> Result<()> {
    let db = Db::open()?;
    for path in db.recent_repos(count.unwrap_or(20))? {
        outln!("{path}");
    }
    Ok(())
}
