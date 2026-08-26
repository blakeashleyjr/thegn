//! `thegn map` — a ranked, line-budgeted **repo map** of a worktree: the
//! tree-sitter-indexed entities (functions, types, …) grouped by file,
//! most-referenced first. The outline coding agents inject for context, exposed
//! outside the interactive Symbols panel — over the CLI here, and over MCP
//! (`semantic.map`).
//!
//! Reads the worktree entity index ([`crate::repo_index`]); when it is empty and
//! no compositor has crawled the worktree, the verb builds a capped index inline
//! (its own process's time). Pure rendering lives in
//! [`thegn_core::repo_map`] — this shell only resolves the worktree, reads the
//! store, and prints.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::outln;

use super::{emit_json, resolve_worktree};

/// Run `thegn map`.
pub fn run(
    cfg: &Config,
    worktree: Option<String>,
    budget: Option<usize>,
    file: Option<String>,
    json: bool,
) -> Result<()> {
    let root = resolve_worktree(worktree);
    let budget = budget.unwrap_or_else(|| cfg.semantic.budget()).max(1);
    let cap = cfg.semantic.file_cap();

    let db = thegn_core::db::Db::open()?;
    let load = crate::repo_index::load_repo_map(&root, cap, &db, file.as_deref());

    if json {
        let rows = load.map.rows(budget);
        emit_json(&serde_json::json!({
            "worktree": root.to_string_lossy(),
            "has_indexable_files": load.has_ts_files,
            "partial": load.map.partial(),
            "total": load.map.total(),
            "shown": rows.len(),
            "rows": rows,
        }))?;
        return Ok(());
    }

    if !load.has_ts_files {
        outln!(
            "no tree-sitter-served files in {} (rust/ts/tsx/js/py/go)",
            root.display()
        );
        return Ok(());
    }
    if load.map.is_empty() {
        // Has indexable files but none carry a named entity (e.g. all config /
        // data modules) — say so rather than printing a bare map.
        outln!("no named entities indexed in {}", root.display());
        return Ok(());
    }
    outln!("{}", load.map.render(budget));
    Ok(())
}
