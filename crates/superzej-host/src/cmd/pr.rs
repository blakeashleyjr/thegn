//! `superzej pr <action>` — GitHub PR data + actions for a worktree.
//!
//! `status` prints a human summary (and warms the panel cache the native host
//! reads). The mutating actions shell out via core's `github` module. The old
//! zellij `pr watch`/`--json` panel-feed paths are gone: the native host polls
//! `github::pr_status` in-process (`run.rs` `spawn_pr_cache_refresh`).

use anyhow::Result;
use superzej_core::db::Db;
use superzej_core::github::{self, CreateOpts, MergeMethod, PanelState, PrPanel};
use superzej_core::remote::GitLoc;
use superzej_core::store::CacheStore;
use superzej_core::{msg, outln};

use crate::cmd::{confirm, resolve_worktree};

/// PR subcommands, mirroring the user-facing half of the legacy `PrAction`
/// (the plugin-only `watch` + `--json` feeds were dropped with the panel WASM).
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// PR + checks + review state (human summary).
    Status {
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Create a PR for the worktree's branch.
    Create {
        #[arg(long)]
        worktree: Option<String>,
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
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Approve the PR.
    Approve {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        body: Option<String>,
    },
    /// Merge the PR.
    Merge {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long, value_enum, default_value_t = MergeMethod::Squash)]
        method: MergeMethod,
        #[arg(long)]
        delete_branch: bool,
        #[arg(long)]
        auto: bool,
    },
    /// Re-run failed checks.
    RerunChecks {
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Print the PR's reviews as JSON.
    Reviews {
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Mark the PR ready for review (or `--undo` back to draft).
    Ready {
        #[arg(long)]
        worktree: Option<String>,
        /// Convert the PR back to a draft instead of marking it ready.
        #[arg(long)]
        undo: bool,
    },
    /// Enable (or `--disable`) auto-merge for the PR.
    AutoMerge {
        #[arg(long)]
        worktree: Option<String>,
        /// Disable auto-merge instead of enabling it.
        #[arg(long)]
        disable: bool,
    },
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Status { worktree } => status(worktree),
        Action::Create {
            worktree,
            title,
            body,
            base,
            draft,
            web,
            fill,
        } => create(worktree, title, body, base, draft, web, fill),
        Action::Open { worktree } => open(worktree),
        Action::Approve { worktree, body } => approve(worktree, body),
        Action::Merge {
            worktree,
            method,
            delete_branch,
            auto,
        } => merge(worktree, method, delete_branch, auto),
        Action::RerunChecks { worktree } => rerun(worktree),
        Action::Reviews { worktree } => reviews(worktree),
        Action::Ready { worktree, undo } => ready(worktree, undo),
        Action::AutoMerge { worktree, disable } => auto_merge(worktree, disable),
    }
}

fn status(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let panel = github::pr_status(&loc);
    let json = serde_json::to_string(&panel).unwrap_or_default();
    if let Ok(db) = Db::open() {
        let _ = db.put_pr_cache(&loc.path(), &panel.branch, &json);
    }
    print_summary(&panel);
    Ok(())
}

fn print_summary(p: &PrPanel) {
    match &p.state {
        PanelState::NoGh => outln!("gh CLI not installed"),
        PanelState::NotAuthenticated => outln!("gh not authenticated (run: gh auth login)"),
        PanelState::NoPr => outln!(
            "branch '{}': no PR yet  (create: superzej pr create)",
            p.branch
        ),
        PanelState::RateLimited => outln!("GitHub API rate limited; try again shortly"),
        PanelState::Offline => outln!("GitHub unreachable (network error)"),
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
    match github::create_pr(&loc, &opts) {
        Ok(out) => {
            if !out.is_empty() {
                outln!("{out}");
            }
            msg::info("PR created");
        }
        Err(e) => msg::die(&format!("pr create failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn open(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if let Err(e) = github::open_pr(&loc) {
        msg::die(&format!("pr open failed: {}", github::describe(&e)));
    }
    Ok(())
}

fn approve(worktree: Option<String>, body: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match github::approve_pr(&loc, body.as_deref()) {
        Ok(()) => msg::info("PR approved"),
        Err(e) => msg::die(&format!("pr approve failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn merge(
    worktree: Option<String>,
    method: MergeMethod,
    delete_branch: bool,
    auto: bool,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if !confirm(&format!("Merge this PR ({method:?})?")) {
        msg::info("cancelled");
        return Ok(());
    }
    match github::merge_pr(&loc, method, delete_branch, auto) {
        Ok(()) => msg::info("PR merged"),
        Err(e) => msg::die(&format!("pr merge failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn rerun(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match github::rerun_failed_checks(&loc) {
        Ok(0) => msg::info("no failed checks to re-run"),
        Ok(n) => msg::info(&format!("re-ran {n} failed workflow run(s)")),
        Err(e) => msg::die(&format!("pr rerun-checks failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn reviews(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match github::reviews(&loc) {
        Ok(json) => outln!("{json}"),
        Err(e) => msg::die(&format!("pr reviews failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn ready(worktree: Option<String>, undo: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    // `undo` converts the PR back to a draft; otherwise mark it ready.
    match github::set_draft_pr(&loc, undo) {
        Ok(()) if undo => msg::info("PR converted to draft"),
        Ok(()) => msg::info("PR marked as ready for review"),
        Err(e) => msg::die(&format!("pr ready failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn auto_merge(worktree: Option<String>, disable: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match github::set_auto_merge(&loc, !disable) {
        Ok(()) if disable => msg::info("auto-merge disabled"),
        Ok(()) => msg::info("auto-merge enabled"),
        Err(e) => msg::die(&format!("pr auto-merge failed: {}", github::describe(&e))),
    }
    Ok(())
}
