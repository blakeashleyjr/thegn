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
use superzej_core::db::Db;
use superzej_core::store::WorkspaceStore;
use superzej_core::{outln, util, worktree};

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
    /// Emit one JSON array instead of the human table.
    #[arg(long)]
    pub json: bool,
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
    /// Create a worktree headlessly (no sandbox prep — the compositor
    /// prepares lazily on first open). Prints the new worktree's absolute
    /// path as its only plain output, so `cd $(superzej wt new x)` works.
    New {
        /// Branch-name tail (the configured prefix + numbering scheme are
        /// applied); omitted = a generated candidate name.
        name: Option<String>,
        /// Repo to create in (default: resolved from cwd / $SUPERZEJ_WORKTREE).
        #[arg(long)]
        repo: Option<String>,
        /// Base ref (default: the configured/auto-resolved base branch).
        #[arg(long)]
        base: Option<String>,
        /// Pin a named execution env (`[env.<name>]`) for the new worktree.
        #[arg(long)]
        env: Option<String>,
        /// Emit the created worktree as one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Remove a worktree: provider/sandbox teardown, `git worktree remove`,
    /// DB cleanup (teardown can take a while on slow container runtimes).
    Rm {
        /// Worktree path or branch name.
        target: String,
        /// Also delete the branch (`git branch -D`).
        #[arg(long)]
        delete_branch: bool,
        /// Skip the confirmation prompt (teardown still runs).
        #[arg(long)]
        force: bool,
    },
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
        Action::New {
            name,
            repo,
            base,
            env,
            json,
        } => new(cfg, name, repo, base, env, json),
        Action::Rm {
            target,
            delete_branch,
            force,
        } => rm(cfg, &target, delete_branch, force),
        Action::Diff(a) => super::diff::run(a.worktree, a.base, a.stat, a.file),
        Action::Disk(a) => super::disk::disk(cfg, a.worktree, a.all, a.json),
        Action::Clean(a) => super::disk::clean(cfg, a.worktree, a.all, a.force),
    }
}

/// `wt new` — the TUI wizard's creation pipeline (wizard.rs `run_worker`)
/// minus UI and sandbox prep: name → base → `git worktree add` → DB register.
fn new(
    cfg: &Config,
    name: Option<String>,
    repo: Option<String>,
    base: Option<String>,
    env: Option<String>,
    json: bool,
) -> Result<()> {
    let start = super::resolve_worktree(repo);
    let Some(root) = superzej_core::repo::main_worktree(&start) else {
        return Err(anyhow::Error::new(super::NotFound(format!(
            "not a git repo: {}",
            start.display()
        ))));
    };

    // A --env must name a defined environment (or the implicit "default").
    if let Some(e) = env.as_deref()
        && e != "default"
        && !cfg.env.contains_key(e)
    {
        let mut known: Vec<&str> = cfg.env.keys().map(String::as_str).collect();
        known.sort_unstable();
        return Err(anyhow::Error::new(super::NotFound(format!(
            "no [env.{e}] defined (known: default{}{})",
            if known.is_empty() { "" } else { ", " },
            known.join(", ")
        ))));
    }

    let branch = worktree::branch_name(&root, name.as_deref(), cfg);
    let base = base
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| worktree::resolve_base(&root, cfg));
    if util::git_out(&root, &["rev-parse", "--verify", "--quiet", &base]).is_none() {
        anyhow::bail!("'{base}' has no commits yet — make an initial commit first");
    }

    let path = worktree::worktree_path(&root, &branch, cfg);
    worktree::add_checked(&root, &branch, &base, &path, cfg).map_err(|e| {
        // Roll the speculative checkout back so a failed create leaves nothing.
        worktree::remove(&root, &path, &branch, true);
        anyhow::anyhow!(e)
    })?;

    // Register (git stays the source of truth; the DB row is what the sidebar
    // + session resurrection read). put_worktree is the primary path; the env
    // pin is a bare UPDATE after it.
    let root_s = root.to_string_lossy().into_owned();
    let path_s = path.to_string_lossy().into_owned();
    let tab = superzej_core::repo::branch_tab(&superzej_core::repo::repo_slug(&root), &branch);
    let db = Db::open()?;
    if let Err(e) = db.put_worktree(&tab, &root_s, &path_s, &branch, None, None) {
        worktree::remove(&root, &path, &branch, true);
        return Err(anyhow::anyhow!("db: {e}"));
    }
    // Pin the env only when it differs from the ambient default this worktree
    // would inherit anyway (same rule as the wizard: a matching choice stays
    // NULL for a clean inherit).
    if let Some(e) = env.as_deref()
        && e != crate::wizard::default_env_name(cfg, &root)
    {
        // best-effort: the worktree exists; a missed pin re-resolves ambient.
        let _ = db.set_worktree_env(&path_s, e);
    }

    if json {
        #[derive(serde::Serialize)]
        struct Created<'a> {
            branch: &'a str,
            path: &'a str,
            root: &'a str,
            base: &'a str,
        }
        return super::emit_json(&Created {
            branch: &branch,
            path: &path_s,
            root: &root_s,
            base: &base,
        });
    }
    outln!("{path_s}");
    Ok(())
}

/// `wt rm` — the TUI's `delete_groups` pipeline, synchronous: resolve →
/// confirm → provider/sandbox teardown → `git worktree remove` → DB cleanup.
fn rm(cfg: &Config, target: &str, delete_branch: bool, force: bool) -> Result<()> {
    let db = Db::open()?;
    let rows = db.worktrees()?;

    // Resolve by exact path first, then unique branch name.
    let target_path = std::fs::canonicalize(target)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| target.to_string());
    let matches: Vec<_> = rows
        .iter()
        .filter(|w| w.worktree == target_path || w.branch == target)
        .collect();
    let (path, branch, repo_root) = match matches.as_slice() {
        [w] => (
            w.worktree.clone(),
            w.branch.clone(),
            (!w.repo_root.is_empty()).then(|| w.repo_root.clone()),
        ),
        [] => {
            // Not registered — accept a live linked worktree by path (the DB
            // is a cache; git is the source of truth).
            let p = std::path::Path::new(&target_path);
            match superzej_core::repo::main_worktree(p) {
                Some(r) if p.is_dir() && p.join(".git").is_file() => {
                    let b = util::git_out(p, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                        .unwrap_or_default();
                    (
                        target_path.clone(),
                        b,
                        Some(r.to_string_lossy().into_owned()),
                    )
                }
                _ => {
                    let mut known: Vec<&str> = rows.iter().map(|w| w.branch.as_str()).collect();
                    known.sort_unstable();
                    return Err(anyhow::Error::new(super::NotFound(format!(
                        "no worktree matches '{target}' (known branches: {})",
                        if known.is_empty() {
                            "none".into()
                        } else {
                            known.join(", ")
                        }
                    ))));
                }
            }
        }
        many => {
            let paths: Vec<&str> = many.iter().map(|w| w.worktree.as_str()).collect();
            anyhow::bail!(
                "'{target}' is ambiguous — pass a path instead: {}",
                paths.join(", ")
            );
        }
    };

    let root_s = repo_root
        .or_else(|| {
            superzej_core::repo::main_worktree(std::path::Path::new(&path))
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| path.clone());
    let root = std::path::PathBuf::from(&root_s);
    if root_s == path {
        anyhow::bail!("refusing to remove the main worktree: {path}");
    }
    if !force
        && !super::confirm(&format!(
            "remove worktree {path} (branch {branch}{})?",
            if delete_branch {
                ", branch deleted"
            } else {
                ""
            }
        ))
    {
        outln!("aborted");
        return Ok(());
    }

    // Provider/sandbox teardown, synchronous (unlike the TUI's fire-and-forget
    // thread — a CLI exiting would orphan it). Same env-precedence resolution
    // as `delete_groups`: DB selection → repo `.superzej.*` → global default,
    // so a repo-selected provider env doesn't leak its sandbox.
    let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(&path));
    let selected = db.effective_env(&path, &root_s);
    let env = cfg.resolve_env(
        &root,
        &loc,
        std::path::Path::new(&path),
        selected.as_deref(),
    );
    if !env.placement.is_local() {
        outln!("tearing down {} sandbox…", env.name);
        crate::agent::destroy_provider_sandbox(&path, &env.name);
    }
    crate::agent::deregister_vpn(&path);
    crate::agent::deproject(&path);
    crate::agent::deprovision_sync(&path);
    crate::agent::checkpoint_on_close(&path);
    superzej_core::sandbox::teardown_by_path(&path);

    // git removal (worktree::remove has the --force fallback), then make sure
    // the directory is actually gone — a lingering dir is re-adopted at next
    // launch and looks like a failed delete.
    worktree::remove(
        &root,
        std::path::Path::new(&path),
        if delete_branch { &branch } else { "" },
        delete_branch,
    );
    let _ = std::fs::remove_dir_all(&path);
    if std::path::Path::new(&path).exists() {
        anyhow::bail!("could not remove {path}");
    }

    // DB cleanup (best-effort: the DB is a cache; git above was the truth).
    let tab = superzej_core::repo::branch_tab(&superzej_core::repo::repo_slug(&root), &branch);
    let _ = db.del_worktree(&path);
    let _ = db.del_worktree_for_tab(&root_s, &tab);
    // Session id == the workspace repo path; key tab-group rows by worktree
    // path so a renamed display group can't leave a resurrecting row behind.
    let _ = db.delete_tab_groups_for_worktree(&root_s, &path);

    outln!("removed {path}");
    Ok(())
}
