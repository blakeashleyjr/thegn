//! `superzej workspaces` — a flat TSV inventory of every *managed* repo, one per
//! line: `repo_slug<TAB>display_name<TAB>repo_path`, newest-active first.
//!
//! It exists for the sidebar plugin (WASM, no DB access): the plugin pulls this
//! over zellij's `run_command` bridge to list repos that aren't currently open
//! as tabs, and to map a tab's `{repo_slug}/…` prefix to a human name. Open
//! repos come from the live `TabUpdate`; this fills in the rest. Plain TSV keeps
//! the plugin parser dependency-free.

use crate::db::Db;
use crate::repo;
use crate::util;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

pub fn run() -> Result<()> {
    let db = Db::open()?;

    // Display names recorded for repos (workspace name overrides the bare slug).
    let names: HashMap<String, String> = db
        .workspaces()?
        .into_iter()
        .map(|w| (w.repo_path, w.name))
        .collect();

    let mut repos = db.known_repos()?;
    repos.sort();
    repos.dedup();

    // (slug, name, path) per repo. Reuse the one open DB connection and derive
    // names from the path basename — no per-repo `Db::open()` or `git` spawn, so
    // this stays instant as the repo count grows (it's pulled on every tab
    // change). `repo::repo_slug`/`repo_name` would reopen the DB and shell out to
    // git for each repo; the paths here are already repo roots, so we don't need
    // either.
    let rows: Vec<(String, String, String)> = repos
        .into_iter()
        .map(|path| {
            let p = Path::new(&path);
            // Slug `base` always derives from the repo dir name (matches the old
            // `repo_slug` path) so persisted slugs stay stable; the display name
            // may be a custom workspace name and shouldn't drive the slug.
            let base = {
                let s = util::slugify(&repo::repo_name_from_path(p));
                if s.is_empty() {
                    "repo".to_string()
                } else {
                    s
                }
            };
            let slug = db.slug_for_repo(&path, &base).unwrap_or(base);
            let name = names
                .get(&path)
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| repo::repo_name_from_path(p));
            (slug, name, path)
        })
        .collect();

    // Disambiguate identical display names (e.g. two `WASHU` checkouts) by
    // appending the parent directory, so the sidebar can tell them apart.
    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for (_, name, _) in &rows {
        *name_counts.entry(name.as_str()).or_insert(0) += 1;
    }

    for (slug, name, path) in &rows {
        let disp = if name_counts.get(name.as_str()).copied().unwrap_or(0) > 1 {
            match Path::new(path).parent().and_then(|p| p.file_name()) {
                Some(parent) => format!("{name} ({})", parent.to_string_lossy()),
                None => name.clone(),
            }
        } else {
            name.clone()
        };
        crate::outln!("{slug}\t{disp}\t{path}");
    }
    Ok(())
}
