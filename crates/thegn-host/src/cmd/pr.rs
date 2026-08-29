//! `thegn pr <action>` — pull-request data + actions for a worktree, through
//! the forge seam (`thegn_core::forge::Forge`, resolved per worktree by the
//! process's `ForgeSet`).
//!
//! `status` prints a human summary (and warms the panel cache the native host
//! reads); the native host refreshes the same feed in-process
//! (`hydrate.rs` `spawn_pr_cache_refresh`).

use anyhow::Result;
use thegn_core::db::Db;
use thegn_core::forge::model::{CreateOpts, MergeMethod, PanelState, PrPanel, ReviewState};
use thegn_core::forge::{PrDepth, PrRef};
use thegn_core::remote::GitLoc;
use thegn_core::store::CacheStore;
use thegn_core::{msg, outln};

use crate::cmd::{confirm, resolve_worktree};

/// PR subcommands, mirroring the user-facing half of the legacy `PrAction`
/// (the plugin-only `watch` + `--json` feeds were dropped with the panel WASM).
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// PR + checks + review state (human summary).
    Status {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Create a PR for the worktree's branch.
    Create {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        draft: bool,
        #[arg(long)]
        web: bool,
        #[arg(long)]
        fill: bool,
    },
    /// Open the PR in a browser.
    Open {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Approve the PR.
    Approve {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        body: Option<String>,
    },
    /// Merge the PR.
    Merge {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long, value_enum, default_value_t = MergeMethod::Squash)]
        method: MergeMethod,
        #[arg(long)]
        delete_branch: bool,
        #[arg(long)]
        auto: bool,
        /// Skip the confirmation prompt (required for non-interactive/CI use).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Re-run failed checks.
    RerunChecks {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Print the PR's reviews as JSON.
    Reviews {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Post a PR-level comment.
    Comment {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        body: String,
    },
    /// Submit a review (approve / request-changes / comment).
    Review {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long, value_enum)]
        state: ReviewState,
        /// Required for request-changes / comment.
        #[arg(long)]
        body: Option<String>,
    },
    /// Print the PR's unified diff (or `--json` for the parsed structure).
    Diff {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        json: bool,
    },
    /// Mark the PR ready for review (or `--undo` back to draft).
    Ready {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Convert the PR back to a draft instead of marking it ready.
        #[arg(long)]
        undo: bool,
    },
    /// Enable (or `--disable`) auto-merge for the PR.
    AutoMerge {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Disable auto-merge instead of enabling it.
        #[arg(long)]
        disable: bool,
    },
    /// The PR queue: shepherd pull requests on the forge — poll them, classify
    /// what is blocking them, optionally hand a blocker to an agent, and let the
    /// forge merge them once green (`[pr_queue]`).
    Queue {
        #[command(subcommand)]
        action: super::pr_queue::Action,
    },
}

pub fn run(cfg: &thegn_core::config::Config, action: Action) -> Result<()> {
    match action {
        // The only verb needing config: the queue is configurable per repo.
        Action::Queue { action } => super::pr_queue::run(cfg, action),
        Action::Status { target } => status(target.worktree),
        Action::Create {
            target,
            title,
            body,
            base,
            draft,
            web,
            fill,
        } => create(target.worktree, title, body, base, draft, web, fill),
        Action::Open { target } => open(target.worktree),
        Action::Approve { target, body } => approve(target.worktree, body),
        Action::Merge {
            target,
            method,
            delete_branch,
            auto,
            yes,
        } => merge(target.worktree, method, delete_branch, auto, yes),
        Action::RerunChecks { target } => rerun(target.worktree),
        Action::Reviews { target } => reviews(target.worktree),
        Action::Comment { target, body } => comment(target.worktree, body),
        Action::Review {
            target,
            state,
            body,
        } => review(target.worktree, state, body),
        Action::Diff { target, json } => diff(target.worktree, json),
        Action::Ready { target, undo } => ready(target.worktree, undo),
        Action::AutoMerge { target, disable } => auto_merge(target.worktree, disable),
    }
}

/// The process's forge set — each verb routes its worktree through `for_loc`.
fn forges() -> std::sync::Arc<thegn_svc::forge::ForgeSet> {
    crate::forge_handle::get()
}

fn status(worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree);
    let loc = GitLoc::for_worktree(&wt);
    let panel = forges()
        .for_loc(&loc)
        .pr_panel(&loc, PrRef::Current, PrDepth::Header);
    let json = serde_json::to_string(&panel).unwrap_or_default();
    if let Ok(db) = Db::open() {
        // Host-path cache key — matches the panel's reader (never `loc.path()`).
        let _ = db.put_pr_cache(&GitLoc::worktree_cache_key(&wt), &panel.branch, &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    print_summary(&panel);
    Ok(())
}

fn print_summary(p: &PrPanel) {
    match &p.state {
        PanelState::NoGh => outln!("forge CLI not installed (gh)"),
        PanelState::NotAuthenticated => outln!("forge not authenticated (run: gh auth login)"),
        PanelState::NoPr => outln!(
            "branch '{}': no PR yet  (create: thegn pr create)",
            p.branch
        ),
        PanelState::RateLimited => outln!("forge API rate limited; try again shortly"),
        PanelState::Offline => outln!("forge unreachable (network error)"),
        PanelState::Error { message } => outln!("error: {message}"),
        PanelState::Pr(pr) => {
            let draft = if pr.is_draft { " (draft)" } else { "" };
            outln!("#{} {}{}  [{}]", pr.number, pr.title, draft, pr.state);
            outln!(
                "  checks: {} ok / {} failed / {} pending   review: {}",
                pr.checks.passed,
                pr.checks.failed,
                pr.checks.pending,
                pr.review_decision.as_deref().unwrap_or("—")
            );
            outln!("  {}", pr.url);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create(
    worktree: Option<String>,
    title: Option<String>,
    body: Option<String>,
    base: Option<String>,
    draft: bool,
    web: bool,
    fill: bool,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let fill = fill || (title.is_none() && body.is_none() && !web);
    let opts = CreateOpts {
        title,
        body,
        base,
        draft,
        web,
        fill,
    };
    match forges().for_loc(&loc).create_pr(&loc, &opts) {
        Ok(out) => {
            if !out.is_empty() {
                outln!("{out}");
            }
            msg::info("PR created");
        }
        Err(e) => msg::die(&format!("pr create failed: {}", e.describe())),
    }
    Ok(())
}

fn open(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if let Err(e) = forges().for_loc(&loc).open_in_browser(&loc, None) {
        msg::die(&format!("pr open failed: {}", e.describe()));
    }
    Ok(())
}

fn approve(worktree: Option<String>, body: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).submit_review(
        &loc,
        PrRef::Current,
        ReviewState::Approve,
        body.as_deref(),
    ) {
        Ok(()) => msg::info("PR approved"),
        Err(e) => msg::die(&format!("pr approve failed: {}", e.describe())),
    }
    Ok(())
}

fn merge(
    worktree: Option<String>,
    method: MergeMethod,
    delete_branch: bool,
    auto: bool,
    yes: bool,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    // Confirmation is required unless `--yes` is passed. Without a TTY (CI /
    // piped scripts) a stdin prompt can't be answered, so refuse with a
    // non-zero exit rather than silently cancelling and reporting success —
    // scripts branch on the exit code.
    if !yes {
        let prompt = format!("Merge this PR ({method:?})?");
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!("{prompt} refusing to merge without confirmation — pass --yes");
        }
        if !confirm(&prompt) {
            anyhow::bail!("merge cancelled");
        }
    }
    match forges()
        .for_loc(&loc)
        .merge_pr(&loc, PrRef::Current, method, delete_branch, auto)
    {
        Ok(()) => msg::info("PR merged"),
        Err(e) => msg::die(&format!("pr merge failed: {}", e.describe())),
    }
    Ok(())
}

fn rerun(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).rerun_failed(&loc, PrRef::Current) {
        Ok(0) => msg::info("no failed checks to re-run"),
        Ok(n) => msg::info(&format!("re-ran {n} failed workflow run(s)")),
        Err(e) => msg::die(&format!("pr rerun-checks failed: {}", e.describe())),
    }
    Ok(())
}

fn reviews(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).reviews_json(&loc) {
        Ok(json) => outln!("{json}"),
        Err(e) => msg::die(&format!("pr reviews failed: {}", e.describe())),
    }
    Ok(())
}

fn comment(worktree: Option<String>, body: String) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).comment(&loc, PrRef::Current, &body) {
        Ok(()) => msg::info("comment posted"),
        Err(e) => msg::die(&format!("pr comment failed: {}", e.describe())),
    }
    Ok(())
}

fn review(worktree: Option<String>, state: ReviewState, body: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges()
        .for_loc(&loc)
        .submit_review(&loc, PrRef::Current, state, body.as_deref())
    {
        Ok(()) => msg::info("review submitted"),
        Err(e) => msg::die(&format!("pr review failed: {}", e.describe())),
    }
    Ok(())
}

fn diff(worktree: Option<String>, json: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if json {
        match forges().for_loc(&loc).pr_diff(&loc, PrRef::Current) {
            Ok(d) => outln!("{}", serde_json::to_string_pretty(&d).unwrap_or_default()),
            Err(e) => msg::die(&format!("pr diff failed: {}", e.describe())),
        }
    } else {
        // Raw unified diff text from the forge.
        match forges().for_loc(&loc).pr_diff_raw(&loc, PrRef::Current) {
            Ok(raw) => outln!("{raw}"),
            Err(e) => msg::die(&format!("pr diff failed: {}", e.describe())),
        }
    }
    Ok(())
}

fn ready(worktree: Option<String>, undo: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    // `undo` converts the PR back to a draft; otherwise mark it ready.
    match forges().for_loc(&loc).set_draft(&loc, PrRef::Current, undo) {
        Ok(()) if undo => msg::info("PR converted to draft"),
        Ok(()) => msg::info("PR marked as ready for review"),
        Err(e) => msg::die(&format!("pr ready failed: {}", e.describe())),
    }
    Ok(())
}

fn auto_merge(worktree: Option<String>, disable: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges()
        .for_loc(&loc)
        .set_auto_merge(&loc, PrRef::Current, !disable, MergeMethod::Squash)
    {
        Ok(()) if disable => msg::info("auto-merge disabled"),
        Ok(()) => msg::info("auto-merge enabled"),
        Err(e) => msg::die(&format!("pr auto-merge failed: {}", e.describe())),
    }
    Ok(())
}
