//! `superzej wt` — the worktree noun-verb namespace.
//!
//! Worktrees are superzej's core noun; this namespace gives them the same
//! grammar every other noun (`pr`, `env`, `host`, …) already has, plus the
//! headless lifecycle (`new`/`rm`) the TUI wizard owns interactively. The
//! legacy bare verbs (`list`, `diff`, `disk`, `clean`) stay functional as
//! hidden top-level commands; both spellings share these arg structs and
//! dispatch to the same functions, so they cannot drift.

use anyhow::Result;
use superzej_core::config::Config;

/// Args shared by `diff` and `wt diff`.
#[derive(clap::Args, Clone)]
pub struct DiffArgs {
    #[arg(long)]
    pub worktree: Option<String>,
    /// Diff against this base ref (default: the repo's default branch).
    #[arg(long)]
    pub base: Option<String>,
    /// Summary (--stat) only.
    #[arg(long)]
    pub stat: bool,
    /// Full diff of a single file.
    #[arg(long)]
    pub file: Option<String>,
}

/// Args shared by `disk` and `wt disk`.
#[derive(clap::Args, Clone)]
pub struct DiskArgs {
    /// Scan only this worktree (defaults to all known worktrees).
    #[arg(long)]
    pub worktree: Option<String>,
    /// Scan every known worktree (the default when no `--worktree` is given).
    #[arg(long)]
    pub all: bool,
}

/// Args shared by `clean` and `wt clean`.
#[derive(clap::Args, Clone)]
pub struct CleanArgs {
    /// Clean this worktree (defaults to the current one).
    #[arg(long)]
    pub worktree: Option<String>,
    /// Clean every known worktree (except the active one).
    #[arg(long)]
    pub all: bool,
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub force: bool,
}

/// Args shared by `list` and `wt list`.
#[derive(clap::Args, Clone)]
pub struct ListArgs {
    /// Emit one JSON array instead of the human table.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List managed worktrees, reconciled against git.
    List(ListArgs),
    /// Emit a syntax-highlighted diff of a worktree against its branch point.
    Diff(DiffArgs),
    /// Report per-worktree disk usage (checkout + reclaimable `target/`).
    Disk(DiskArgs),
    /// Reclaim a worktree's `target/` build artifacts (keeps the checkout).
    Clean(CleanArgs),
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List(a) => super::list::run(cfg, a.json),
        Action::Diff(a) => super::diff::run(a.worktree, a.base, a.stat, a.file),
        Action::Disk(a) => super::disk::disk(cfg, a.worktree, a.all),
        Action::Clean(a) => super::disk::clean(cfg, a.worktree, a.all, a.force),
    }
}
