//! `thegn wt` — the worktree noun-verb namespace.
//!
//! Worktrees are thegn's core noun; this namespace gives them the same
//! grammar every other noun (`pr`, `env`, `host`, …) already has, plus the
//! headless lifecycle (`new`/`rm`) the TUI wizard owns interactively. The
//! legacy bare verbs (`list`, `diff`, `disk`, `clean`) stay functional as
//! hidden top-level commands; both spellings share these arg structs and
//! dispatch to the same functions, so they cannot drift.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::WorkspaceStore;
use thegn_core::{msg, outln, util, worktree};

/// Args shared by `diff` and `wt diff`.
#[derive(clap::Args, Clone)]
pub struct DiffArgs {
    #[command(flatten)]
    pub target: super::target::WorktreeFlag,
    /// Diff against this base ref (default: the repo's default branch).
    #[arg(long)]
    pub base: Option<String>,
    /// Summary (--stat) only.
    #[arg(long)]
    pub stat: bool,
    /// Full diff of a single file.
    #[arg(long)]
    pub file: Option<String>,
    /// Render structurally (difftastic) instead of the internal unified view.
    /// A read-only view — never fed to `git apply`. Falls back to the internal
    /// highlighter with a notice when difft is unavailable.
    #[arg(long)]
    pub structural: bool,
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
    /// path as its only plain output, so `cd $(thegn wt new x)` works.
    New {
        /// Branch-name tail (the configured prefix + numbering scheme are
        /// applied); omitted = a generated candidate name. Required with
        /// `--program` (the feature's linked branch name).
        name: Option<String>,
        /// Repo to create in (default: resolved from cwd / $THEGN_WORKTREE).
        #[arg(long)]
        repo: Option<String>,
        /// Base ref (default: the configured/auto-resolved base branch).
        #[arg(long)]
        base: Option<String>,
        /// Pin a named execution env (`[env.<name>]`) for the new worktree.
        #[arg(long)]
        env: Option<String>,
        /// Create the feature across a program's member repos: one resolved
        /// branch name + a worktree in each member (see `thegn program`).
        #[arg(long, visible_alias = "project")]
        program: Option<String>,
        /// With `--program`, restrict to a comma-separated subset of member
        /// repos (by name), e.g. `--repos api,web`.
        #[arg(long)]
        repos: Option<String>,
        /// Emit the created worktree(s) as one JSON object.
        /// Create from a tracker issue id (`"<provider>:<key>"`): derive the
        /// branch from the issue's hint and link the issue to the worktree —
        /// the headless twin of the panel's `s`/`D` keys (THE-57).
        #[arg(long)]
        from_issue: Option<String>,
        /// Emit the created worktree as one JSON object.
        #[arg(long)]
        json: bool,
        /// File the new worktree into this repo-scoped sidebar folder.
        #[arg(long)]
        folder: Option<String>,
    },
    /// File a worktree into a repo-scoped sidebar folder (or clear it).
    Folder {
        /// Worktree path (or a directory inside it).
        worktree: String,
        /// Folder name; an existing folder is matched case-insensitively.
        name: Option<String>,
        /// Remove the worktree from its folder without deleting the folder.
        #[arg(long, conflicts_with = "name")]
        clear: bool,
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
            program,
            repos,
            from_issue,
            json,
            folder,
        } => match program {
            Some(p) => new_batched(cfg, name, &p, repos, base, env, folder, json),
            None => new(cfg, name, repo, base, env, from_issue, folder, json),
        },
        Action::Folder {
            worktree,
            name,
            clear,
        } => folder(&worktree, name.as_deref(), clear),
        Action::Rm {
            target,
            delete_branch,
            force,
        } => rm(cfg, &target, delete_branch, force),
        Action::Diff(a) => {
            super::diff::run(cfg, a.target.worktree, a.base, a.stat, a.file, a.structural)
        }
        Action::Disk(a) => super::disk::disk(cfg, a.worktree, a.all, a.json),
        Action::Clean(a) => super::disk::clean(cfg, a.worktree, a.all, a.force),
    }
}

/// `wt new` — the TUI wizard's creation pipeline (wizard.rs `run_worker`)
/// minus UI and sandbox prep: name → base → `git worktree add` → DB register.
#[allow(clippy::too_many_arguments)]
fn new(
    cfg: &Config,
    name: Option<String>,
    repo: Option<String>,
    base: Option<String>,
    env: Option<String>,
    from_issue: Option<String>,
    folder: Option<String>,
    json: bool,
) -> Result<()> {
    let folder = folder.as_deref().map(validate_folder_name).transpose()?;
    let start = super::resolve_worktree(repo);
    let Some(root) = thegn_core::repo::main_worktree(&start) else {
        return Err(anyhow::Error::new(super::NotFound(format!(
            "not a git repo: {}",
            start.display()
        ))));
    };

    // `--from-issue` derives the branch from the tracker issue (the same
    // `issue_branch_seed` rule the `D` key and `worktrees.create` use) and links
    // the issue after registration, so the three doors cannot drift. It ignores
    // any positional `name`.
    let issue_id = from_issue.filter(|s| !s.trim().is_empty());
    let issue_branch = match &issue_id {
        Some(id) => Some(resolve_issue_branch(cfg, &root, id)?),
        None => None,
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

    let branch = match issue_branch {
        Some(b) => b,
        None => worktree::branch_name(&root, name.as_deref(), cfg),
    };
    let base = base
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| worktree::resolve_base(&root, cfg));
    if util::git_out(&root, &["rev-parse", "--verify", "--quiet", &base]).is_none() {
        anyhow::bail!("'{base}' has no commits yet — make an initial commit first");
    }

    let db = Db::open()?;
    let (path_s, filing_error) = create_and_register(
        cfg,
        &root,
        &branch,
        &base,
        env.as_deref(),
        folder.as_deref(),
        &db,
    )?;
    let root_s = root.to_string_lossy().into_owned();

    // Link the issue so the tab carries its badge — the same link the `D` key
    // records (best-effort: the worktree is already registered).
    if let Some(id) = &issue_id {
        use thegn_core::store::WorktreeAuxStore;
        let _ = db.link_issue(&path_s, id); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }

    if json {
        #[derive(serde::Serialize)]
        struct Created<'a> {
            branch: &'a str,
            path: &'a str,
            root: &'a str,
            base: &'a str,
        }
        super::emit_json(&Created {
            branch: &branch,
            path: &path_s,
            root: &root_s,
            base: &base,
        })?;
    } else {
        outln!("{path_s}");
    }
    // The worktree exists and is fully set up; only the folder label failed.
    // Report the path first so a caller still learns where it is, then fail.
    match filing_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Create + register one worktree for an ALREADY-resolved branch name and a
/// verified base. Shared by the single-repo `wt new` and the batched `--project`
/// path so both run the identical pipeline (`git worktree add` → seed mq assets
/// → DB register → env pin) and cannot drift. Rolls the speculative checkout
/// back on any failure so a failed create leaves nothing. Returns the created
/// worktree's absolute path plus a HELD folder-filing error: filing is the last
/// step (after the env pin and PostCreate hooks) and a failure there must not
/// skip any setup, so the caller reports the path and then surfaces the error.
/// Everything about filing that can be known up front is checked before the
/// worktree is created (`check_folder_fileable`).
fn create_and_register(
    cfg: &Config,
    root: &std::path::Path,
    branch: &str,
    base: &str,
    env: Option<&str>,
    folder: Option<&str>,
    db: &Db,
) -> Result<(String, Option<anyhow::Error>)> {
    // A creation that is going to fail filing for a knowable reason (bad name,
    // removed workspace) must not create anything.
    let folder = match folder {
        Some(name) => Some(check_folder_fileable(db, &root.to_string_lossy(), name)?),
        None => None,
    };
    // THE-516: fallible identity resolution — never a basename/slug alias.
    // The slug comes from the caller's OWN handle (`db`), not a second
    // connection: authority must not depend on a duplicate open.
    let path = worktree::allocate_worktree_path(root, branch, cfg)?;
    let workspace = thegn_core::repo::repo_slug_with_checked(db, root)?;
    let pre = crate::worktree_lifecycle::run_event_with_db(
        cfg,
        root,
        &path,
        branch,
        &workspace,
        thegn_core::hooks::HookEvent::PreCreate,
        thegn_core::hooks::HookExecutionMode::User,
        Some(db),
    );
    if pre.blocked() {
        return Err(anyhow::anyhow!(pre.message()));
    }
    worktree::add_checked_with_state(root, branch, base, &path, cfg).map_err(|e| {
        // Roll the speculative checkout back so a failed create leaves nothing.
        let message = crate::worktree_lifecycle::create_failure_after_add(
            e.message.clone(),
            cfg,
            root,
            &path,
            branch,
            &e,
        );
        anyhow::anyhow!(message)
    })?;
    if let Err(e) = crate::git_worktree::initialize(cfg, root, &path, None) {
        msg::warn(&e);
    }
    // Seed the configured skill registry in each harness-native layout. This
    // non-interactive CLI path may do the bounded work synchronously.
    crate::skill_seed::seed_if_enabled(cfg, &path, thegn_core::skills::SeedPhase::Create);

    // Register (git stays the source of truth; the DB row is what the sidebar
    // + session resurrection read). put_worktree is the primary path; the env
    // pin upserts after it.
    let root_s = root.to_string_lossy().into_owned();
    let path_s = path.to_string_lossy().into_owned();
    let tab = thegn_core::repo::branch_tab(&workspace, branch);
    if let Err(e) = db.put_worktree(&tab, &root_s, &path_s, branch, None, None) {
        let message = match crate::worktree_lifecycle::rollback_remove(cfg, root, &path, branch) {
            Ok(()) => format!("db: {e}"),
            Err(cleanup) => format!("db: {e}; rollback failed: {cleanup}"),
        };
        return Err(anyhow::anyhow!(message));
    }
    // Pin the env only when it differs from the ambient default this worktree
    // would inherit anyway (same rule as the wizard: a matching choice stays
    // NULL for a clean inherit).
    if let Some(e) = env
        && e != crate::wizard::ambient_env_name(Some(db), cfg, root)
    {
        // best-effort: the worktree exists; a missed pin re-resolves ambient.
        let _ = db.set_worktree_env(&path_s, e);
    }
    // A CLI has no compositor to keep alive, so it waits for post-create
    // completion before printing success and exiting. Warn-only failures are
    // reported by the lifecycle runner but do not roll back a real worktree.
    let post = crate::worktree_lifecycle::run_event_with_db(
        cfg,
        root,
        &path,
        branch,
        &workspace,
        thegn_core::hooks::HookEvent::PostCreate,
        thegn_core::hooks::HookExecutionMode::User,
        Some(db),
    );
    if post.blocked() {
        let message = crate::worktree_lifecycle::create_failure_with_rollback(
            format!("post_create: {}", post.message()),
            cfg,
            root,
            &path,
            branch,
        );
        let _ = db.del_worktree(&path_s);
        return Err(anyhow::anyhow!(message));
    }
    // File last. A filing failure does NOT roll back the worktree: registration
    // failing above leaves it untracked and unusable, so unwinding that is
    // right, but a folder is a sidebar label and destroying a real worktree (and
    // its branch) over one is far worse than an unfiled row. The error is held
    // so the env pin and hooks above have already run.
    let filing_error = folder.and_then(|folder_name| {
        file_registered_worktree(db, &root_s, &path_s, &folder_name)
            .err()
            .map(|e| {
                anyhow::anyhow!(
                    "{e}; the worktree at {path_s} was created, registered and set up, \
                     but NOT filed into {folder_name:?}; retry the filing with \
                     `thegn wt folder {path_s:?} {folder_name:?}` once the cause above is fixed"
                )
            })
    });
    Ok((path_s, filing_error))
}

/// Validate before touching the folder table: `ensure_folder` intentionally
/// accepts empty strings for its lower-level callers.
fn validate_folder_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("folder name must not be empty");
    }
    Ok(name.to_string())
}

/// Everything about filing that is knowable before a worktree exists: the name
/// is valid, and the workspace has not been explicitly removed (a tombstoned
/// workspace is honoured rather than resurrected by filing, and the refusal
/// persists, so creating a worktree first would strand it unfiled). Returns the
/// trimmed folder name.
fn check_folder_fileable(db: &Db, repo_path: &str, folder_name: &str) -> Result<String> {
    let folder_name = validate_folder_name(folder_name)?;
    if db.workspace_tombstoned(repo_path).unwrap_or(false) {
        anyhow::bail!(
            "workspace {repo_path} was removed from thegn; re-add it before filing \
             its worktrees into a folder"
        );
    }
    Ok(folder_name)
}

/// Find or create the repo-scoped folder, then use the identity-checked write.
/// Reading the row back is required because the DB match is trimmed and
/// case-insensitive while the identity guard deliberately compares exactly.
fn file_registered_worktree(
    db: &Db,
    repo_path: &str,
    worktree_path: &str,
    folder_name: &str,
) -> Result<()> {
    let folder_name = check_folder_fileable(db, repo_path, folder_name)?;
    // `folders.repo_path` REFERENCES `workspaces(repo_path)`, and that row is
    // otherwise only ever written by the compositor's hydration — so filing from
    // the CLI against a repo that has never been opened in the TUI would fail
    // with a raw `FOREIGN KEY constraint failed`. Register it first, mirroring
    // hydrate's name/kind rule. A workspace the user explicitly removed is
    // tombstoned: honour that instead of resurrecting it as a side effect of
    // filing a folder.
    let repo = std::path::Path::new(repo_path);
    let workspace_name = repo
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());
    let kind = if thegn_core::repo::main_worktree(repo).is_some() {
        "repo"
    } else {
        "dir"
    };
    db.put_workspace(repo_path, &workspace_name, kind)?;
    let folder_id = db.ensure_folder(repo_path, &folder_name)?;
    let folder = db
        .folders_for_workspace(repo_path)?
        .into_iter()
        .find(|folder| folder.folder_id == folder_id && folder.repo_path == repo_path)
        .ok_or_else(|| anyhow::anyhow!("folder disappeared before worktree filing"))?;
    if !db.set_worktree_folder_if_identity(worktree_path, repo_path, folder_id, &folder.name)? {
        anyhow::bail!("worktree or folder identity changed before filing");
    }
    Ok(())
}

/// Register only when the row is genuinely absent, so filing also works for a
/// worktree not yet in the sidebar cache. An existing row keeps its identity:
/// `put_worktree` rewrites `tab_name`/`branch` from the live branch, which would
/// rename a drifted worktree's tab (and can trip the THE-516 ambiguity check
/// against a stale sibling). The caller supplies identity discovered from Git.
fn register_and_file_worktree(
    db: &Db,
    repo_path: &str,
    worktree_path: &str,
    branch: &str,
    folder_name: &str,
) -> Result<()> {
    if db.worktree_record(worktree_path)?.is_none() {
        let slug = thegn_core::repo::repo_slug_with_checked(db, std::path::Path::new(repo_path))?;
        let tab = thegn_core::repo::branch_tab(&slug, branch);
        db.put_worktree(&tab, repo_path, worktree_path, branch, None, None)?;
    }
    file_registered_worktree(db, repo_path, worktree_path, folder_name)
}

/// `--clear`: unfile an existing row without ever creating one. Returns whether
/// a row existed.
fn clear_folder_if_registered(db: &Db, worktree_path: &str) -> Result<bool> {
    if db.worktree_record(worktree_path)?.is_none() {
        return Ok(false);
    }
    db.set_worktree_folder(worktree_path, None)?;
    Ok(true)
}

/// Resolve the supplied directory through Git and file/unfile that actual
/// worktree. A plain directory or a non-Git path is an explicit refusal.
fn folder(target: &str, name: Option<&str>, clear: bool) -> Result<()> {
    if !clear && name.is_none() {
        anyhow::bail!("provide a folder name or use --clear");
    }
    let folder_name = name.map(validate_folder_name).transpose()?;
    let target = super::resolve_worktree(Some(target.to_string()));
    let Some(worktree_root) = thegn_core::repo::worktree_root_for_cwd(&target) else {
        anyhow::bail!("not a git worktree: {}", target.display());
    };
    let Some(repo_root) = thegn_core::repo::main_worktree(&worktree_root) else {
        anyhow::bail!("not a git worktree: {}", target.display());
    };
    let repo_path = repo_root.to_string_lossy().into_owned();
    let worktree_path = worktree_root.to_string_lossy().into_owned();
    let branch = util::git_out(&worktree_root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .or_else(|| {
            util::git_out(&worktree_root, &["rev-parse", "--short", "HEAD"])
                .map(|commit| format!("detached-{commit}"))
        })
        .ok_or_else(|| anyhow::anyhow!("could not determine worktree identity"))?;
    let db = Db::open()?;
    if let Some(folder_name) = folder_name {
        register_and_file_worktree(&db, &repo_path, &worktree_path, &branch, &folder_name)?;
        outln!("Filed {worktree_path} into folder \"{folder_name}\"");
    } else {
        // Never register on --clear: an unregistered worktree has no folder.
        if clear_folder_if_registered(&db, &worktree_path)? {
            outln!("Unfiled {worktree_path}");
        } else {
            outln!("Nothing to clear: {worktree_path} is not registered, so it is in no folder");
        }
    }
    Ok(())
}

/// `wt new --program <p>` — batched cross-repo feature creation. Resolves ONE
/// linked branch name (prefix + slug, applied once — per-repo prefix overrides
/// are NOT re-applied, so identity is literal) and creates that exact branch +
/// worktree in each member repo (or a `--repos` subset), running the same
/// per-repo pipeline independently. Per-member outcomes are reported; a failure
/// never rolls back siblings, and a re-run attaches (reports `exists`) members
/// that already have the branch — so retry-after-partial-failure completes the
/// set. Exits non-zero if any member failed.
// One call site, and every argument is a distinct CLI flag forwarded verbatim —
// a struct would only rename the same fields. Same judgement as `config_write`.
#[allow(clippy::too_many_arguments)]
fn new_batched(
    cfg: &Config,
    name: Option<String>,
    program_name: &str,
    repos: Option<String>,
    base: Option<String>,
    env: Option<String>,
    folder: Option<String>,
    json: bool,
) -> Result<()> {
    use thegn_core::project::{self, MemberBranchState, MemberPlan};
    use thegn_core::store::ProjectStore;

    let Some(feature) = name.filter(|n| !n.trim().is_empty()) else {
        anyhow::bail!(
            "a --program feature needs a name: `thegn wt new <name> --program {program_name}`"
        );
    };
    let folder = folder.as_deref().map(validate_folder_name).transpose()?;

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

    let db = Db::open()?;
    let proj = db
        .list_projects()?
        .into_iter()
        .find(|p| p.name == program_name)
        .ok_or_else(|| {
            super::NotFound(format!(
                "no program named {program_name:?} (create it with `thegn program create {program_name}`)"
            ))
        })?;
    let members = db.project_members(proj.project_id)?;
    if members.is_empty() {
        anyhow::bail!(
            "program {program_name} has no member repos — assign some with \
             `thegn program assign {program_name} <repo>`"
        );
    }

    // Resolve the single, literal branch name ONCE (no per-repo dedup), then
    // probe each member for it (exact existence) to classify create vs attach.
    let branch = project::feature_branch_name(&feature, &cfg.branch_prefix)
        .map_err(|e| anyhow::anyhow!(e))?;
    let states: Vec<MemberBranchState> = members
        .iter()
        .map(|(root, repo_name)| MemberBranchState {
            repo_root: root.clone(),
            repo_name: repo_name.clone(),
            has_branch: worktree::branch_exists(std::path::Path::new(root), &branch),
        })
        .collect();

    let repos_filter: Option<Vec<String>> = repos.map(|s| {
        s.split(',')
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect()
    });
    let plan = project::plan_batched_create(&branch, &states, repos_filter.as_deref());

    // Execute member by member — each independent, no rollback of siblings.
    #[derive(serde::Serialize)]
    struct MemberOutcome {
        repo: String,
        status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }
    let mut outcomes: Vec<MemberOutcome> = Vec::with_capacity(plan.members.len());
    for m in &plan.members {
        match m.plan {
            MemberPlan::Exists => outcomes.push(MemberOutcome {
                repo: m.repo_name.clone(),
                status: "exists",
                path: None,
                error: None,
            }),
            MemberPlan::Create => {
                let root = std::path::PathBuf::from(&m.repo_root);
                let resolved_base = base
                    .as_ref()
                    .filter(|b| !b.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| worktree::resolve_base(&root, cfg));
                if util::git_out(&root, &["rev-parse", "--verify", "--quiet", &resolved_base])
                    .is_none()
                {
                    outcomes.push(MemberOutcome {
                        repo: m.repo_name.clone(),
                        status: "failed",
                        path: None,
                        error: Some(format!("base '{resolved_base}' has no commits")),
                    });
                    continue;
                }
                match create_and_register(
                    cfg,
                    &root,
                    &branch,
                    &resolved_base,
                    env.as_deref(),
                    folder.as_deref(),
                    &db,
                ) {
                    Ok((path, None)) => outcomes.push(MemberOutcome {
                        repo: m.repo_name.clone(),
                        status: "created",
                        path: Some(path),
                        error: None,
                    }),
                    // Only the filing step failed: the worktree is real, so
                    // report where it is rather than hiding it.
                    Ok((path, Some(e))) => outcomes.push(MemberOutcome {
                        repo: m.repo_name.clone(),
                        status: "failed",
                        path: Some(path),
                        error: Some(e.to_string()),
                    }),
                    Err(e) => outcomes.push(MemberOutcome {
                        repo: m.repo_name.clone(),
                        status: "failed",
                        path: None,
                        error: Some(e.to_string()),
                    }),
                }
            }
        }
    }

    let any_failed = outcomes.iter().any(|o| o.status == "failed");

    if json {
        // The field is a borrowed slice, so serde hands the predicate a
        // `&&[String]` — `Vec::is_empty` does not apply to it.
        fn no_unknown_repos(v: &&[String]) -> bool {
            v.is_empty()
        }
        #[derive(serde::Serialize)]
        struct Report<'a> {
            project: &'a str,
            branch: &'a str,
            members: &'a [MemberOutcome],
            #[serde(skip_serializing_if = "no_unknown_repos")]
            unknown_repos: &'a [String],
        }
        super::emit_json(&Report {
            project: program_name,
            branch: &branch,
            members: &outcomes,
            unknown_repos: &plan.unknown_repos,
        })?;
    } else {
        outln!("program {program_name}: feature branch {branch}");
        for o in &outcomes {
            match (o.status, &o.path, &o.error) {
                ("created", Some(p), _) => outln!("  {} created  {}", o.repo, p),
                ("exists", ..) => outln!("  {} exists   (attached)", o.repo),
                ("failed", Some(p), Some(e)) => outln!("  {} FAILED   {} ({})", o.repo, e, p),
                ("failed", _, Some(e)) => outln!("  {} FAILED   {}", o.repo, e),
                _ => outln!("  {} {}", o.repo, o.status),
            }
        }
        for u in &plan.unknown_repos {
            outln!("  (warning) --repos {u:?} matches no member of {program_name}");
        }
    }

    if any_failed {
        // Non-zero exit so scripts detect a partial set and re-run to attach the
        // succeeded members. The per-member report above already named each.
        anyhow::bail!("one or more members failed — re-run to attach the succeeded members");
    }
    Ok(())
}

/// Resolve the branch a `--from-issue` worktree should take: fetch the issue
/// from the configured tracker, derive the seed branch ([`thegn_core::issue::issue_branch_seed`]),
/// then de-duplicate against the repo's existing branches — exactly the `D` key
/// / `worktrees.create` derivation, so the doors cannot drift.
fn resolve_issue_branch(cfg: &Config, root: &std::path::Path, issue_id: &str) -> Result<String> {
    // Repo-resolved so the repo overlay's restrictions/pins (account filter,
    // team id) apply; the overlay can only narrow, never add accounts.
    let router = thegn_svc::issue::IssueRouter::from_config(&cfg.repo_issues(Some(root)));
    if !router.is_configured() {
        anyhow::bail!("no issue tracker configured (set [issues] providers/accounts)");
    }
    let rt = tokio::runtime::Runtime::new()?;
    let detail = rt
        .block_on(router.get_issue(issue_id))
        .map_err(|e| anyhow::anyhow!("fetch issue {issue_id}: {e}"))?;
    let seed = thegn_core::issue::issue_branch_seed(
        detail.issue.branch_hint.as_deref(),
        &detail.issue.number,
    );
    let taken = worktree::BranchSet::load(root);
    Ok(worktree::dedupe(&seed, &taken))
}

fn unreadable_note(unreadable_roots: &[String]) -> String {
    if unreadable_roots.is_empty() {
        String::new()
    } else {
        format!(
            " (could not read repositories: {})",
            unreadable_roots.join(", ")
        )
    }
}

/// The NotFound text. It lists the LIVE branches Git reported — registry
/// branches are creation metadata and can no longer match — and names any repo
/// root that could not be read, since the target may live there.
fn no_match_message(target: &str, live_branches: &[String], unreadable_roots: &[String]) -> String {
    let mut known: Vec<&str> = live_branches.iter().map(String::as_str).collect();
    known.sort_unstable();
    known.dedup();
    format!(
        "no worktree matches '{target}' (live branches: {}){}",
        if known.is_empty() {
            "none".into()
        } else {
            known.join(", ")
        },
        unreadable_note(unreadable_roots)
    )
}

/// `--delete-branch` runs `git branch -D` (force, no merged check) on the LIVE
/// branch, so it is refused unless that is provably the branch this worktree
/// was created for, and never for the repository's default branch or the one
/// the main worktree has checked out.
fn check_branch_deletable(
    live: &str,
    recorded: Option<&str>,
    default_branch: Option<&str>,
    main_checked_out: Option<&str>,
) -> Result<()> {
    if live.is_empty() {
        return Ok(());
    }
    if let Some(recorded) = recorded.filter(|r| !r.is_empty() && *r != live) {
        anyhow::bail!(
            "refusing --delete-branch: the worktree was created on branch '{recorded}' but \
             currently has '{live}' checked out. Check out '{recorded}' and rerun, or delete \
             the branch by hand"
        );
    }
    if default_branch == Some(live) || main_checked_out == Some(live) {
        anyhow::bail!(
            "refusing --delete-branch: '{live}' is the repository's default / main-worktree \
             branch; delete it by hand if you really mean to"
        );
    }
    Ok(())
}

/// `wt rm` — the TUI's `delete_groups` pipeline, synchronous: resolve →
/// confirm → provider/sandbox teardown → `git worktree remove` → DB cleanup.
fn rm(cfg: &Config, target: &str, delete_branch: bool, force: bool) -> Result<()> {
    let db = Db::open()?;
    let rows = db.worktrees()?;

    // Resolve by exact path first, then by Git's current worktree snapshot.
    // `WorktreeRow.branch` is creation metadata and must not select a target.
    let target_path = std::fs::canonicalize(target)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| target.to_string());
    let path_match = rows.iter().find(|row| {
        std::fs::canonicalize(&row.worktree)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| row.worktree.clone())
            == target_path
    });
    // Observe live branches. A repo root git cannot read (stale rows whose repo
    // is gone) must not sink the whole command: its rows become `Unavailable`
    // and we only fail if the final match is empty or ambiguous, naming the
    // roots we could not read. Remote rows never appear in a local `git worktree
    // list`, so they are read through their location; this is an explicit CLI
    // verb (not hydration), so one live read per remote row is acceptable.
    let candidates: Vec<&thegn_core::models::WorktreeRow> = match path_match {
        Some(row) => vec![row],
        None => rows.iter().collect(),
    };
    let is_remote = |row: &thegn_core::models::WorktreeRow| {
        thegn_core::remote::GitLoc::from_db(&row.worktree, Some(&row.location)).is_remote()
    };
    let mut observations: std::collections::HashMap<
        String,
        crate::worktree_snapshot::BranchObservation,
    > = std::collections::HashMap::new();
    let mut unreadable_roots: Vec<String> = Vec::new();
    let mut local_by_root: std::collections::BTreeMap<&str, Vec<thegn_core::models::WorktreeRow>> =
        Default::default();
    let mut remote_rows: Vec<thegn_core::models::WorktreeRow> = Vec::new();
    for row in &candidates {
        if is_remote(row) {
            remote_rows.push((*row).clone());
        } else if !row.repo_root.is_empty() {
            local_by_root
                .entry(row.repo_root.as_str())
                .or_default()
                .push((*row).clone());
        }
    }
    for (root, root_rows) in local_by_root {
        match thegn_svc::git::GitBackend::worktrees(
            &*crate::git_handle::get(),
            std::path::Path::new(root),
        ) {
            Ok(snapshot) => observations.extend(crate::worktree_snapshot::join_snapshot(
                &root_rows, &snapshot,
            )),
            Err(_) => unreadable_roots.push(root.to_string()),
        }
    }
    // Local rows are resolved first. A local match short-circuits the remote
    // reads: each is an ssh/provider exec bounded only by `git_read_timeout`
    // (60 s), so a dead host would otherwise stall removing a LOCAL worktree.
    // Skipping them means a remote worktree holding the same branch name is not
    // weighed against the local match; that is acceptable because removal needs
    // an unambiguous match and a local one the user can see is still unique
    // among local rows (the ambiguity check below stays exact for those), while
    // the remote row stays addressable by path.
    let local_hit = path_match.is_some()
        || candidates.iter().any(|row| {
            !is_remote(row)
                && observations.get(&row.worktree).and_then(|o| o.branch()) == Some(target)
        });
    if !local_hit && !remote_rows.is_empty() {
        observations.extend(crate::worktree_snapshot::observe_rows(
            &*crate::git_handle::get(),
            &remote_rows,
            crate::worktree_snapshot::RemoteRead::Live,
        ));
    }
    let live_branch = |row: &thegn_core::models::WorktreeRow| {
        observations
            .get(&row.worktree)
            .and_then(|observation| observation.branch().map(str::to_owned))
    };
    let matches: Vec<_> = path_match.map(|row| vec![row]).unwrap_or_else(|| {
        rows.iter()
            .filter(|row| live_branch(row).as_deref() == Some(target))
            .collect()
    });
    // (path, live branch, repo root, the matched row's own tab, recorded branch)
    let (path, branch, repo_root, row_tab, recorded_branch) = match matches.as_slice() {
        [w] => {
            let branch = live_branch(w);
            if delete_branch && branch.is_none() {
                anyhow::bail!(
                    "cannot delete branch for {target_path}: Git did not provide a live branch"
                );
            }
            (
                w.worktree.clone(),
                branch.unwrap_or_default(),
                (!w.repo_root.is_empty()).then(|| w.repo_root.clone()),
                Some(w.tab_name.clone()).filter(|t| !t.is_empty()),
                Some(w.branch.clone()),
            )
        }
        [] => {
            // Not registered — accept a live linked worktree by path (the DB
            // is a cache; git is the source of truth).
            let p = std::path::Path::new(&target_path);
            match thegn_core::repo::main_worktree(p) {
                Some(r) if p.is_dir() && p.join(".git").is_file() => {
                    let b = util::git_out(p, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                        .unwrap_or_default();
                    (
                        target_path.clone(),
                        b,
                        Some(r.to_string_lossy().into_owned()),
                        None,
                        None,
                    )
                }
                _ => {
                    return Err(anyhow::Error::new(super::NotFound(no_match_message(
                        target,
                        &candidates
                            .iter()
                            .filter_map(|row| live_branch(row))
                            .collect::<Vec<_>>(),
                        &unreadable_roots,
                    ))));
                }
            }
        }
        many => {
            let paths: Vec<&str> = many.iter().map(|w| w.worktree.as_str()).collect();
            anyhow::bail!(
                "'{target}' is ambiguous — pass a path instead: {}{}",
                paths.join(", "),
                unreadable_note(&unreadable_roots)
            );
        }
    };

    if delete_branch {
        let root_path = repo_root
            .as_deref()
            .map(std::path::PathBuf::from)
            .or_else(|| thegn_core::repo::main_worktree(std::path::Path::new(&path)));
        let (default_branch, main_checked_out) = match &root_path {
            Some(root) => (
                Some(worktree::default_branch(root)),
                util::git_out(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]),
            ),
            None => (None, None),
        };
        check_branch_deletable(
            &branch,
            recorded_branch.as_deref(),
            default_branch.as_deref(),
            main_checked_out.as_deref(),
        )?;
    }

    let root_s = repo_root
        .or_else(|| {
            thegn_core::repo::main_worktree(std::path::Path::new(&path))
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| path.clone());
    let root = std::path::PathBuf::from(&root_s);
    if root_s == path {
        anyhow::bail!("refusing to remove the main worktree: {path}");
    }
    if !force {
        let prompt = format!(
            "remove worktree {path} (branch {branch}{})?",
            if delete_branch {
                ", branch deleted"
            } else {
                ""
            }
        );
        // Without a TTY there's no way to answer the prompt — refuse (non-zero)
        // rather than silently no-op on a piped/scripted invocation that forgot
        // --force; an interactive decline is a clean abort.
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!("{prompt} refusing without a TTY — pass --force to confirm");
        }
        if !super::confirm(&prompt) {
            outln!("aborted");
            return Ok(());
        }
    }

    let workspace = thegn_core::repo::repo_slug(&root);
    // Keep the CLI and TUI on the same transaction. This is synchronous so a
    // CLI exit cannot orphan provider resources, while `--force` selects the
    // explicit non-blocking hook policy.
    let (removed, message) = crate::worktree_lifecycle::destroy_one(
        cfg,
        &root,
        std::path::Path::new(&path),
        &branch,
        &workspace,
        false,
        delete_branch,
        crate::worktree_lifecycle::mode_for_user(force, false),
        Some(&db),
    );
    if !removed {
        anyhow::bail!("{message}; retry with --force");
    }

    // DB cleanup (best-effort: the DB is a cache; git above was the truth).
    // The matched row's OWN tab, never one rebuilt from the live branch: a
    // sibling worktree created as `feat-b` and later checked out elsewhere
    // still owns tab `app/feat-b`, and a branch-derived name would delete it.
    let _ = db.del_worktree(&path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    if let Some(tab) = &row_tab {
        let _ = db.del_worktree_for_tab(&root_s, tab); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    // Session id == the workspace repo path; key tab-group rows by worktree
    // path so a renamed display group can't leave a resurrecting row behind.
    let _ = db.delete_tab_groups_for_worktree(&root_s, &path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth

    outln!("removed {path}");
    Ok(())
}

#[cfg(test)]
mod rm_tests {
    use super::*;

    #[test]
    fn delete_branch_refuses_when_live_branch_differs_from_recorded() {
        let err = check_branch_deletable("main", Some("tg/x"), Some("main"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("tg/x") && err.contains("main"), "{err}");
        assert!(err.contains("refusing --delete-branch"), "{err}");
    }

    #[test]
    fn delete_branch_never_touches_default_or_main_worktree_branch() {
        // Even when recorded == live (or unrecorded), the default branch and
        // the branch the main worktree holds are never force-deleted here.
        assert!(check_branch_deletable("main", Some("main"), Some("main"), None).is_err());
        assert!(check_branch_deletable("trunk", None, Some("main"), Some("trunk")).is_err());
        assert!(check_branch_deletable("tg/x", Some("tg/x"), Some("main"), Some("main")).is_ok());
        assert!(check_branch_deletable("tg/x", None, None, None).is_ok());
    }

    #[test]
    fn no_match_message_lists_live_branches_and_unreadable_roots() {
        let msg = no_match_message(
            "feat-x",
            &["b".into(), "a".into(), "a".into()],
            &["/gone".into()],
        );
        assert!(msg.contains("live branches: a, b"), "{msg}");
        assert!(msg.contains("could not read repositories: /gone"), "{msg}");
        assert!(no_match_message("t", &[], &[]).contains("none"));
    }
}

#[cfg(test)]
mod folder_tests {
    use super::{
        check_folder_fileable, clear_folder_if_registered, file_registered_worktree,
        register_and_file_worktree, validate_folder_name,
    };
    use thegn_core::db::Db;
    use thegn_core::store::WorkspaceStore;

    fn parse(args: &[&str]) -> Result<crate::Cli, clap::Error> {
        use clap::Parser;
        crate::Cli::try_parse_from(args)
    }

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    #[test]
    fn folder_names_are_trimmed_and_empty_names_refuse() {
        assert_eq!(validate_folder_name("  Agents  ").unwrap(), "Agents");
        assert!(validate_folder_name(" \t ").is_err());
        let db = db();
        assert!(file_registered_worktree(&db, "/repo", "/repo/wt", "  ").is_err());
        assert!(db.folders_for_workspace("/repo").unwrap().is_empty());
    }

    #[test]
    fn folder_cli_accepts_file_and_clear_forms() {
        let cli = parse(&["thegn", "wt", "folder", "/repo/wt", "Pipeline"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::Command::Wt {
                action: super::Action::Folder {
                    worktree,
                    name: Some(name),
                    clear: false,
                }
            }) if worktree == "/repo/wt" && name == "Pipeline"
        ));

        let cli = parse(&["thegn", "wt", "folder", "/repo/wt", "--clear"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::Command::Wt {
                action: super::Action::Folder {
                    worktree,
                    name: None,
                    clear: true,
                }
            }) if worktree == "/repo/wt"
        ));
    }

    #[test]
    fn wt_new_folder_option_parses_for_single_and_program_creation() {
        for args in [
            &["thegn", "wt", "new", "feature", "--folder", "Pipeline"][..],
            &[
                "thegn",
                "wt",
                "new",
                "feature",
                "--program",
                "batch",
                "--folder",
                "Pipeline",
            ][..],
        ] {
            let cli = parse(args).unwrap();
            assert!(matches!(
                cli.command,
                Some(crate::Command::Wt {
                    action: super::Action::New {
                        folder: Some(ref name),
                        ..
                    }
                }) if name == "Pipeline"
            ));
        }
    }

    #[test]
    fn repeated_filing_is_case_insensitive_and_trimmed() {
        let db = db();
        register_and_file_worktree(&db, "/repo", "/repo/wt", "feature", "  Agents ").unwrap();
        register_and_file_worktree(&db, "/repo", "/repo/wt", "feature", " agents  ").unwrap();

        let folders = db.folders_for_workspace("/repo").unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Agents");
        assert_eq!(
            db.worktree_record("/repo/wt").unwrap().unwrap().folder_id,
            Some(folders[0].folder_id)
        );
    }

    #[test]
    fn absent_worktree_row_is_registered_before_filing() {
        let db = db();
        assert!(db.worktree_record("/repo/new").unwrap().is_none());

        register_and_file_worktree(&db, "/repo", "/repo/new", "new", "Pipeline").unwrap();

        let row = db.worktree_record("/repo/new").unwrap().unwrap();
        assert_eq!(row.repo_root, "/repo");
        assert!(row.folder_id.is_some());
    }

    #[test]
    fn folders_are_repo_scoped_and_foreign_identity_is_rejected() {
        let db = db();
        register_and_file_worktree(&db, "/repo/a", "/repo/a/wt", "same", "Pipeline").unwrap();
        register_and_file_worktree(&db, "/repo/b", "/repo/b/wt", "same", "Pipeline").unwrap();

        let a = db.folders_for_workspace("/repo/a").unwrap();
        let b = db.folders_for_workspace("/repo/b").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_ne!(a[0].folder_id, b[0].folder_id);
        assert!(
            !db.set_worktree_folder_if_identity(
                "/repo/a/wt",
                "/repo/a",
                b[0].folder_id,
                &b[0].name
            )
            .unwrap()
        );
        assert_eq!(
            db.worktree_record("/repo/a/wt").unwrap().unwrap().folder_id,
            Some(a[0].folder_id)
        );
    }

    #[test]
    fn clearing_unfiles_without_deleting_the_folder() {
        let db = db();
        register_and_file_worktree(&db, "/repo", "/repo/wt", "feature", "Pipeline").unwrap();
        let folder = db.folders_for_workspace("/repo").unwrap().remove(0);
        db.set_worktree_folder("/repo/wt", None).unwrap();

        assert_eq!(
            db.worktree_record("/repo/wt").unwrap().unwrap().folder_id,
            None
        );
        assert_eq!(db.folders_for_workspace("/repo").unwrap(), vec![folder]);
    }

    #[test]
    fn filing_an_existing_row_leaves_its_identity_untouched() {
        let db = db();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.put_worktree("repo/orig", "/repo", "/repo/wt", "orig", None, None)
            .unwrap();
        // The live branch has drifted; a stale sibling holds the drifted tab.
        register_and_file_worktree(&db, "/repo", "/repo/wt", "drifted", "Agents").unwrap();
        let row = db.worktree_record("/repo/wt").unwrap().unwrap();
        assert_eq!(row.branch, "orig");
        assert_eq!(row.tab_name, "repo/orig");
        assert!(row.folder_id.is_some());
    }

    #[test]
    fn clearing_an_unregistered_path_creates_no_row() {
        let db = db();
        assert!(!clear_folder_if_registered(&db, "/repo/ghost").unwrap());
        assert!(db.worktree_record("/repo/ghost").unwrap().is_none());

        register_and_file_worktree(&db, "/repo", "/repo/wt", "feature", "Pipeline").unwrap();
        assert!(clear_folder_if_registered(&db, "/repo/wt").unwrap());
        assert_eq!(
            db.worktree_record("/repo/wt").unwrap().unwrap().folder_id,
            None
        );
    }

    #[test]
    fn precheck_refuses_a_tombstoned_workspace_and_bad_names_before_creation() {
        let db = db();
        assert_eq!(
            check_folder_fileable(&db, "/repo", "  Agents ").unwrap(),
            "Agents"
        );
        assert!(check_folder_fileable(&db, "/repo", "  ").is_err());

        db.tombstone_workspace("/gone").unwrap();
        assert!(db.workspace_tombstoned("/gone").unwrap());
        let err = check_folder_fileable(&db, "/gone", "Agents").unwrap_err();
        assert!(err.to_string().contains("was removed from thegn"));
        // Refusing must not have written anything.
        assert!(db.folders_for_workspace("/gone").unwrap().is_empty());
    }

    #[test]
    fn deleting_a_folder_unfiles_its_worktrees() {
        let db = db();
        register_and_file_worktree(&db, "/repo", "/repo/wt", "feature", "Pipeline").unwrap();
        let folder_id = db.folders_for_workspace("/repo").unwrap()[0].folder_id;
        db.del_folder(folder_id).unwrap();

        assert!(db.folders_for_workspace("/repo").unwrap().is_empty());
        assert_eq!(
            db.worktree_record("/repo/wt").unwrap().unwrap().folder_id,
            None
        );
    }
}
