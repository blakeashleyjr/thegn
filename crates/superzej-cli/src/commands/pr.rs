//! `superzej pr <action>` — GitHub PR data + actions for a worktree.
//!
//! `status` is cache-aware (the panel polls it cheaply); `watch` keeps the cache
//! warm and pushes JSON to the panel plugin. The mutating actions (create /
//! merge / approve / rerun) shell out via `github.rs` and print results so the
//! plugin can run them in a floating pane and re-fetch.

use crate::cli::PrAction;
use crate::commands::{confirm, panels, resolve_worktree};
use crate::config::Config;
use crate::db::Db;
use crate::remote::GitLoc;
use crate::{msg, util, zellij};
use anyhow::Result;
use superzej_core::forge::models::{CreateOpts, PanelState, PrPanel};

pub fn run(cfg: &Config, action: PrAction) -> Result<()> {
    match action {
        PrAction::Status {
            worktree,
            json,
            refresh,
        } => status(cfg, worktree, json, refresh),
        PrAction::Watch { worktree, interval } => watch(cfg, worktree, interval),
        PrAction::Create {
            worktree,
            title,
            body,
            base,
            draft,
            web,
            fill,
        } => create(worktree, title, body, base, draft, web, fill),
        PrAction::Open { worktree } => open(worktree),
        PrAction::Approve { worktree, body } => approve(worktree, body),
        PrAction::Merge {
            worktree,
            method,
            delete_branch,
            auto,
        } => merge(worktree, method, delete_branch, auto),
        PrAction::RerunChecks { worktree } => rerun(worktree),
        PrAction::Reviews { worktree } => reviews(worktree),
        PrAction::Draft { worktree, undo } => draft(worktree, undo),
        PrAction::Ready { worktree } => ready(worktree),
        PrAction::AutoMerge { worktree, disable } => auto_merge(worktree, disable),
        PrAction::Logs { worktree, check } => logs(worktree, check),
    }
}

/// The panel JSON, served from cache when fresh (unless `refresh`); otherwise a
/// live `gh` fetch written back to the cache.
fn fetch_json(cfg: &Config, loc: &GitLoc, refresh: bool) -> String {
    let wt_s = loc.path();
    if !refresh {
        if let Ok(db) = Db::open() {
            if let Ok(Some((json, fetched_at))) = db.get_pr_cache(&wt_s) {
                if util::now() - fetched_at < cfg.pr.ttl_secs as i64 {
                    return json; // cache hit — no network
                }
            }
        }
    }
    let panel = superzej_core::forge::get_forge_for_loc(loc)
        .unwrap()
        .pr_status(loc);
    let json = serde_json::to_string(&panel).unwrap_or_default();
    if let Ok(db) = Db::open() {
        let _ = db.put_pr_cache(&wt_s, &panel.branch, &json);
    }
    json
}

fn status(cfg: &Config, worktree: Option<String>, json: bool, refresh: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if json {
        // Cache-served fast path for the plugin.
        crate::outln!("{}", fetch_json(cfg, &loc, refresh));
    } else {
        // Always show a fresh human summary on the CLI.
        let panel = superzej_core::forge::get_forge_for_loc(&loc)
            .unwrap()
            .pr_status(&loc);
        let json = serde_json::to_string(&panel).unwrap_or_default();
        if let Ok(db) = Db::open() {
            let _ = db.put_pr_cache(&loc.path(), &panel.branch, &json);
        }
        print_summary(&panel);
    }
    Ok(())
}

fn print_summary(p: &PrPanel) {
    match &p.state {
        PanelState::NoForgeCli => crate::outln!("Forge CLI not installed"),
        PanelState::NotAuthenticated => crate::outln!("gh not authenticated (run: gh auth login)"),
        PanelState::NoPr => crate::outln!(
            "branch '{}': no PR yet  (create: superzej pr create)",
            p.branch
        ),
        PanelState::RateLimited => crate::outln!("GitHub API rate limited; try again shortly"),
        PanelState::Error { message } => crate::outln!("error: {message}"),
        PanelState::Pr(pr) => {
            let draft = if pr.is_draft { " (draft)" } else { "" };
            crate::outln!("#{} {}{}  [{}]", pr.number, pr.title, draft, pr.state);
            crate::outln!(
                "  checks: {} ok / {} failed / {} pending   review: {}",
                pr.checks.passed,
                pr.checks.failed,
                pr.checks.pending,
                pr.review_decision.as_deref().unwrap_or("—")
            );
            crate::outln!("  {}", pr.url);
        }
    }
}

fn watch(cfg: &Config, worktree: Option<String>, interval: Option<u64>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let url = panels::plugin_url("panel.wasm");
    let base = interval.unwrap_or(cfg.watch.pr_interval_secs).max(1);
    let mut delay = base;
    loop {
        let panel = superzej_core::forge::get_forge_for_loc(&loc)
            .unwrap()
            .pr_status(&loc);
        let json = serde_json::to_string(&panel).unwrap_or_default();
        if let Ok(db) = Db::open() {
            let _ = db.put_pr_cache(&loc.path(), &panel.branch, &json);
        }
        if zellij::in_zellij() {
            zellij::pipe_plugin(&url, "superzej_pr", &json);
        }
        // Back off on rate limits, otherwise hold the base interval.
        delay = match panel.state {
            PanelState::RateLimited => (delay.saturating_mul(2)).min(base.saturating_mul(8)),
            _ => base,
        };
        std::thread::sleep(std::time::Duration::from_secs(delay));
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
    // Default to --fill when nothing else was specified, so the action is usable
    // from a keybind without prompting.
    let fill = fill || (title.is_none() && body.is_none() && !web);
    let opts = CreateOpts {
        title,
        body,
        base,
        draft,
        web,
        fill,
    };
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .create_pr(&loc, &opts)
    {
        Ok(out) => {
            if !out.is_empty() {
                crate::outln!("{out}");
            }
            msg::info("PR created");
        }
        Err(e) => msg::die(&format!(
            "pr create failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn open(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if let Err(e) = superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .open_pr(&loc)
    {
        msg::die(&format!(
            "pr open failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        ));
    }
    Ok(())
}

fn approve(worktree: Option<String>, body: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .approve_pr(&loc, body.as_deref())
    {
        Ok(()) => msg::info("PR approved"),
        Err(e) => msg::die(&format!(
            "pr approve failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn merge(
    worktree: Option<String>,
    method: superzej_core::forge::models::MergeMethod,
    delete_branch: bool,
    auto: bool,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if !confirm(&format!("Merge this PR ({method:?})?")) {
        msg::info("cancelled");
        return Ok(());
    }
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .merge_pr(&loc, method, delete_branch, auto)
    {
        Ok(()) => msg::info("PR merged"),
        Err(e) => msg::die(&format!(
            "pr merge failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn rerun(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .rerun_failed_checks(&loc)
    {
        Ok(0) => msg::info("no failed checks to re-run"),
        Ok(n) => msg::info(&format!("re-ran {n} failed workflow run(s)")),
        Err(e) => msg::die(&format!(
            "pr rerun-checks failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn reviews(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .reviews(&loc)
    {
        Ok(json) => crate::outln!("{json}"),
        Err(e) => msg::die(&format!(
            "pr reviews failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn draft(worktree: Option<String>, undo: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    // undo=true means convert to draft (undo the "ready" action)
    // undo=false means mark as ready for review
    let draft = undo;
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .set_draft(&loc, draft)
    {
        Ok(()) => {
            if draft {
                msg::info("PR converted to draft");
            } else {
                msg::info("PR marked as ready for review");
            }
        }
        Err(e) => msg::die(&format!(
            "pr draft failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn ready(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .set_draft(&loc, false)
    {
        Ok(()) => msg::info("PR marked as ready for review"),
        Err(e) => msg::die(&format!(
            "pr ready failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn auto_merge(worktree: Option<String>, disable: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    // disable=false means enable auto-merge
    // disable=true means disable auto-merge
    let enable = !disable;
    match superzej_core::forge::get_forge_for_loc(&loc)
        .unwrap()
        .set_auto_merge(&loc, enable)
    {
        Ok(()) => {
            if enable {
                msg::info("Auto-merge enabled");
            } else {
                msg::info("Auto-merge disabled");
            }
        }
        Err(e) => msg::die(&format!(
            "pr automerge failed: {}",
            superzej_core::forge::models::ForgeError::message(&e)
        )),
    }
    Ok(())
}

fn logs(worktree: Option<String>, check: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = superzej_core::forge::get_forge_for_loc(&loc)
        .ok_or_else(|| anyhow::anyhow!("No supported forge detected"))?;
    
    let logs_output = forge.get_check_logs(&loc, check.as_deref().unwrap_or("")).map_err(|e| anyhow::anyhow!("failed to retrieve logs: {}", e.message()))?;
    crate::outln!("{}", logs_output);
    Ok(())
}
