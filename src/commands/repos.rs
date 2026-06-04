//! `superzej repos` — git repos discovered under repo_roots (what the picker offers).

use crate::config::Config;
use crate::repo;
use anyhow::Result;

pub fn run(cfg: &Config) -> Result<()> {
    for path in repo::discover_repos(cfg) {
        println!("{path}");
    }
    Ok(())
}
