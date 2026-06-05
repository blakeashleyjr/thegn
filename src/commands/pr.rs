//! `superzej pr <action>` — GitHub PR data + actions for a worktree.
//!
//! `status` is cache-aware (the panel polls it cheaply); `watch` keeps the cache
//! warm and pushes JSON to the panel plugin. The mutating actions (create /
//! merge / approve / rerun) shell out via `github.rs` and print results so the
//! plugin can run them in a floating pane and re-fetch.

use crate::cli::PrAction;
use crate::commands::{confirm, panels, resolve_worktree};
use crate::db::Db;
use crate::github::{self, CreateOpts, PanelState, PrPanel};
use crate::{msg, util, zellij};
use anyhow::Result;
use std::path::Path;

fn ttl() -> i64 {
    std::env::var("SZ_PR_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

pub fn run(action: PrAction) -> Result<()> {
    match action {
        PrAction::Status {
            worktree,
            json,
            refresh,
        } => status(worktree, json, refresh),
        PrAction::Watch { worktree, interval } => watch(worktree, interval),
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
    }
}

/// The panel JSON, served from cache when fresh (unless `refresh`); otherwise a
/// live `gh` fetch written back to the cache.
fn fetch_json(wt: &Path, refresh: bool) -> String {
    let wt_s = wt.to_string_lossy().into_owned();
    if !refresh {
        if let Ok(db) = Db::open() {
            if let Ok(Some((json, fetched_at))) = db.get_pr_cache(&wt_s) {
                if util::now() - fetched_at < ttl() {
                    return json; // cache hit — no network
                }
            }
        }
    }
    let panel = github::pr_status(wt);
    let json = serde_json::to_string(&panel).unwrap_or_default();
    if let Ok(db) = Db::open() {
        let _ = db.put_pr_cache(&wt_s, &panel.branch, &json);
    }
    json
}

fn status(worktree: Option<String>, json: bool, refresh: bool) -> Result<()> {
    let wt = resolve_worktree(worktree);
    if json {
        // Cache-served fast path for the plugin.
        println!("{}", fetch_json(&wt, refresh));
    } else {
        // Always show a fresh human summary on the CLI.
        let panel = github::pr_status(&wt);
        let json = serde_json::to_string(&panel).unwrap_or_default();
        if let Ok(db) = Db::open() {
            let _ = db.put_pr_cache(&wt.to_string_lossy(), &panel.branch, &json);
        }
        print_summary(&panel);
    }
    Ok(())
}

fn print_summary(p: &PrPanel) {
    match &p.state {
        PanelState::NoGh => println!("gh CLI not installed"),
        PanelState::NotAuthenticated => println!("gh not authenticated (run: gh auth login)"),
        PanelState::NoPr => println!(
            "branch '{}': no PR yet  (create: superzej pr create)",
            p.branch
        ),
        PanelState::RateLimited => println!("GitHub API rate limited; try again shortly"),
        PanelState::Error { message } => println!("error: {message}"),
        PanelState::Pr(pr) => {
            let draft = if pr.is_draft { " (draft)" } else { "" };
            println!("#{} {}{}  [{}]", pr.number, pr.title, draft, pr.state);
            println!(
                "  checks: {} ok / {} failed / {} pending   review: {}",
                pr.checks.passed,
                pr.checks.failed,
                pr.checks.pending,
                pr.review_decision.as_deref().unwrap_or("—")
            );
            println!("  {}", pr.url);
        }
    }
}

fn watch(worktree: Option<String>, interval: u64) -> Result<()> {
    let wt = resolve_worktree(worktree);
    let url = panels::plugin_url("panel.wasm");
    let base = interval.max(1);
    let mut delay = base;
    loop {
        let panel = github::pr_status(&wt);
        let json = serde_json::to_string(&panel).unwrap_or_default();
        if let Ok(db) = Db::open() {
            let _ = db.put_pr_cache(&wt.to_string_lossy(), &panel.branch, &json);
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
    let wt = resolve_worktree(worktree);
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
    match github::create_pr(&wt, &opts) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            msg::info("PR created");
        }
        Err(e) => msg::die(&format!("pr create failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn open(worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree);
    if let Err(e) = github::open_pr(&wt) {
        msg::die(&format!("pr open failed: {}", github::describe(&e)));
    }
    Ok(())
}

fn approve(worktree: Option<String>, body: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree);
    match github::approve_pr(&wt, body.as_deref()) {
        Ok(()) => msg::info("PR approved"),
        Err(e) => msg::die(&format!("pr approve failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn merge(
    worktree: Option<String>,
    method: github::MergeMethod,
    delete_branch: bool,
    auto: bool,
) -> Result<()> {
    let wt = resolve_worktree(worktree);
    if !confirm(&format!("Merge this PR ({method:?})?")) {
        msg::info("cancelled");
        return Ok(());
    }
    match github::merge_pr(&wt, method, delete_branch, auto) {
        Ok(()) => msg::info("PR merged"),
        Err(e) => msg::die(&format!("pr merge failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn rerun(worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree);
    match github::rerun_failed_checks(&wt) {
        Ok(0) => msg::info("no failed checks to re-run"),
        Ok(n) => msg::info(&format!("re-ran {n} failed workflow run(s)")),
        Err(e) => msg::die(&format!("pr rerun-checks failed: {}", github::describe(&e))),
    }
    Ok(())
}

fn reviews(worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree);
    match github::reviews(&wt) {
        Ok(json) => println!("{json}"),
        Err(e) => msg::die(&format!("pr reviews failed: {}", github::describe(&e))),
    }
    Ok(())
}
