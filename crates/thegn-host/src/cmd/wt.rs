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
use thegn_core::{outln, util, worktree};

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
        /// `--project` (the feature's linked branch name).
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
        /// Create the feature across a project's member repos: one resolved
        /// branch name + a worktree in each member (see `thegn project`).
        #[arg(long)]
        project: Option<String>,
        /// With `--project`, restrict to a comma-separated subset of member
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
            project,
            repos,
            from_issue,
            json,
        } => match project {
            Some(p) => new_batched(cfg, name, &p, repos, base, env, json),
            None => new(cfg, name, repo, base, env, from_issue, json),
        },
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
    json: bool,
) -> Result<()> {
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
    let path_s = create_and_register(cfg, &root, &branch, &base, env.as_deref(), &db)?;
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

/// Create + register one worktree for an ALREADY-resolved branch name and a
/// verified base. Shared by the single-repo `wt new` and the batched `--project`
/// path so both run the identical pipeline (`git worktree add` → seed mq assets
/// → DB register → env pin) and cannot drift. Rolls the speculative checkout
/// back on any failure so a failed create leaves nothing. Returns the created
/// worktree's absolute path.
fn create_and_register(
    cfg: &Config,
    root: &std::path::Path,
    branch: &str,
    base: &str,
    env: Option<&str>,
    db: &Db,
) -> Result<String> {
    let path = worktree::worktree_path(root, branch, cfg);
    let workspace = thegn_core::repo::repo_slug(root);
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
    worktree::add_checked(root, branch, base, &path, cfg).map_err(|e| {
        // Roll the speculative checkout back so a failed create leaves nothing.
        let primary = e.to_string();
        let message = match crate::worktree_lifecycle::rollback_remove(cfg, root, &path, branch) {
            Ok(()) => primary,
            Err(cleanup) => format!("{primary}; rollback failed: {cleanup}"),
        };
        anyhow::anyhow!(message)
    })?;
    // Seed the bundled merge-queue agent assets (`/mq`, `/mq-add`, `/mq-drain`)
    // so agents launched in this worktree discover them (best-effort, gated on
    // [merge_queue] enabled).
    crate::mq_assets::seed_if_enabled(cfg, &path);

    // Register (git stays the source of truth; the DB row is what the sidebar
    // + session resurrection read). put_worktree is the primary path; the env
    // pin upserts after it.
    let root_s = root.to_string_lossy().into_owned();
    let path_s = path.to_string_lossy().into_owned();
    let tab = thegn_core::repo::branch_tab(&thegn_core::repo::repo_slug(root), branch);
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
    crate::worktree_lifecycle::run_event_with_db(
        cfg,
        root,
        &path,
        branch,
        &workspace,
        thegn_core::hooks::HookEvent::PostCreate,
        thegn_core::hooks::HookExecutionMode::User,
        Some(db),
    );
    Ok(path_s)
}

/// `wt new --project <p>` — batched cross-repo feature creation. Resolves ONE
/// linked branch name (prefix + slug, applied once — per-repo prefix overrides
/// are NOT re-applied, so identity is literal) and creates that exact branch +
/// worktree in each member repo (or a `--repos` subset), running the same
/// per-repo pipeline independently. Per-member outcomes are reported; a failure
/// never rolls back siblings, and a re-run attaches (reports `exists`) members
/// that already have the branch — so retry-after-partial-failure completes the
/// set. Exits non-zero if any member failed.
fn new_batched(
    cfg: &Config,
    name: Option<String>,
    project_name: &str,
    repos: Option<String>,
    base: Option<String>,
    env: Option<String>,
    json: bool,
) -> Result<()> {
    use thegn_core::project::{self, MemberBranchState, MemberPlan};
    use thegn_core::store::ProjectStore;

    let Some(feature) = name.filter(|n| !n.trim().is_empty()) else {
        anyhow::bail!(
            "a --project feature needs a name: `thegn wt new <name> --project {project_name}`"
        );
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

    let db = Db::open()?;
    let proj = db
        .list_projects()?
        .into_iter()
        .find(|p| p.name == project_name)
        .ok_or_else(|| {
            super::NotFound(format!(
                "no project named {project_name:?} (create it with `thegn project create {project_name}`)"
            ))
        })?;
    let members = db.project_members(proj.project_id)?;
    if members.is_empty() {
        anyhow::bail!(
            "project {project_name} has no member repos — assign some with \
             `thegn project assign {project_name} <repo>`"
        );
    }

    // Resolve the single, literal branch name ONCE (no per-repo dedup), then
    // probe each member for it (exact existence) to classify create vs attach.
    let branch = project::feature_branch_name(&feature, &cfg.branch_prefix);
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
                match create_and_register(cfg, &root, &branch, &resolved_base, env.as_deref(), &db)
                {
                    Ok(path) => outcomes.push(MemberOutcome {
                        repo: m.repo_name.clone(),
                        status: "created",
                        path: Some(path),
                        error: None,
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
            project: project_name,
            branch: &branch,
            members: &outcomes,
            unknown_repos: &plan.unknown_repos,
        })?;
    } else {
        outln!("project {project_name}: feature branch {branch}");
        for o in &outcomes {
            match (o.status, &o.path, &o.error) {
                ("created", Some(p), _) => outln!("  {} created  {}", o.repo, p),
                ("exists", ..) => outln!("  {} exists   (attached)", o.repo),
                ("failed", _, Some(e)) => outln!("  {} FAILED   {}", o.repo, e),
                _ => outln!("  {} {}", o.repo, o.status),
            }
        }
        for u in &plan.unknown_repos {
            outln!("  (warning) --repos {u:?} matches no member of {project_name}");
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
    let router = thegn_svc::issue::IssueRouter::from_config(&cfg.issues);
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
            match thegn_core::repo::main_worktree(p) {
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
    let tab = thegn_core::repo::branch_tab(&thegn_core::repo::repo_slug(&root), &branch);
    let _ = db.del_worktree(&path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    let _ = db.del_worktree_for_tab(&root_s, &tab); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    // Session id == the workspace repo path; key tab-group rows by worktree
    // path so a renamed display group can't leave a resurrecting row behind.
    let _ = db.delete_tab_groups_for_worktree(&root_s, &path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth

    outln!("removed {path}");
    Ok(())
}
