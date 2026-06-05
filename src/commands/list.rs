//! `superzej list [--json]` — inventory of managed worktrees, reconciled against
//! git. `collect` is shared with the dashboard.

use crate::config::Config;
use crate::db::Db;
use crate::models::WorktreeView;
use crate::{repo, util, worktree};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

fn is_managed(path: &str, branch: &str, cfg: &Config) -> bool {
    path.starts_with(&cfg.worktrees_dir)
        || path.contains("/.worktrees/")
        || branch.starts_with(&cfg.branch_prefix)
}

/// All superzej-managed worktrees across known repos, with live git status.
pub fn collect(cfg: &Config) -> Result<Vec<WorktreeView>> {
    let db = Db::open()?;

    // DB metadata (created_at, agent) keyed by worktree path.
    let meta: HashMap<String, (i64, String)> = db
        .worktrees()?
        .into_iter()
        .map(|w| (w.worktree, (w.created_at, w.agent)))
        .collect();

    // repo path -> display name (the workspace), for the WORKSPACE column.
    let sessions: HashMap<String, String> = db
        .workspaces()?
        .into_iter()
        .map(|w| (w.repo_path, w.name))
        .collect();

    let mut repos = db.known_repos()?;
    repos.sort();
    repos.dedup();

    let mut out = Vec::new();
    for repo_path in repos {
        let repo_dir = Path::new(&repo_path);
        if !repo_dir.is_dir() {
            continue;
        }
        let name = sessions
            .get(&repo_path)
            .cloned()
            .unwrap_or_else(|| repo::repo_name(repo_dir));
        let base = worktree::default_branch(repo_dir);

        let porcelain = match util::git_out(repo_dir, &["worktree", "list", "--porcelain"]) {
            Some(s) => s,
            None => continue,
        };

        let mut wt_path = String::new();
        let mut wt_branch = String::new();
        for line in porcelain.lines().chain(std::iter::once("")) {
            if let Some(p) = line.strip_prefix("worktree ") {
                wt_path = p.to_string();
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                wt_branch = b.to_string();
            } else if line.is_empty() && !wt_path.is_empty() {
                if wt_path != repo_path && is_managed(&wt_path, &wt_branch, cfg) {
                    out.push(view(&wt_path, &wt_branch, &name, &repo_path, &base, &meta));
                }
                wt_path.clear();
                wt_branch.clear();
            }
        }
    }
    Ok(out)
}

fn view(
    path: &str,
    branch: &str,
    workspace: &str,
    repo_path: &str,
    base: &str,
    meta: &HashMap<String, (i64, String)>,
) -> WorktreeView {
    let p = Path::new(path);
    let exists = p.is_dir();
    let mut dirty = 0;
    let mut ahead = 0;
    let mut behind = 0;
    if exists {
        if let Some(s) = util::git_out(p, &["status", "--porcelain"]) {
            dirty = s.lines().filter(|l| !l.is_empty()).count() as i64;
        }
        // "--left-right --count base...HEAD" => "<behind>\t<ahead>".
        if let Some(s) = util::git_out(
            p,
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{base}...HEAD"),
            ],
        ) {
            let mut it = s.split_whitespace();
            behind = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            ahead = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        }
    }
    let (created_at, agent) = meta.get(path).cloned().unwrap_or((0, String::new()));
    WorktreeView {
        workspace: workspace.to_string(),
        repo: repo_path.to_string(),
        path: path.to_string(),
        branch: branch.to_string(),
        agent,
        dirty,
        ahead,
        behind,
        created_at,
        exists,
    }
}

pub fn run(cfg: &Config, json: bool) -> Result<()> {
    use crate::theme;
    use std::io::IsTerminal;

    let rows = collect(cfg)?;
    if json {
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No worktrees yet. Press Alt-W to open a workspace, Alt-w for a worktree.");
        return Ok(());
    }

    // Color only when writing to a terminal (piped output stays clean).
    let tty = std::io::stdout().is_terminal();
    let accent = cfg.accent_rgb();
    let c = |on: &str, s: &str| -> String {
        if tty {
            format!("\x1b[38;2;{on}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };

    println!(
        "{}",
        c(
            theme::FAINT,
            &format!(
                "{:<16} {:<26} {:>5} {:>4} {:>4} {:>6}  AGENT",
                "WORKSPACE", "BRANCH", "AGE", "+", "-", "FILES"
            )
        )
    );
    for r in rows {
        let ahead = if r.ahead > 0 {
            c(theme::GREEN, &format!("{:>4}", r.ahead))
        } else {
            c(theme::GHOST, &format!("{:>4}", r.ahead))
        };
        let behind = if r.behind > 0 {
            c(theme::RED, &format!("{:>4}", r.behind))
        } else {
            c(theme::GHOST, &format!("{:>4}", r.behind))
        };
        let files = if r.dirty > 0 {
            c(theme::AMBER, &format!("{:>6}", r.dirty))
        } else {
            c(theme::GHOST, &format!("{:>6}", r.dirty))
        };
        // AGENT column: identity glyph chip + name in the agent's hue.
        let agent = if r.agent.is_empty() {
            String::new()
        } else if tty {
            let hue = theme::agent_hue(&r.agent);
            format!(
                "{} {}",
                theme::glyph_square(&theme::agent_glyph(&r.agent), hue),
                c(hue, &r.agent)
            )
        } else {
            r.agent.clone()
        };
        println!(
            "{} {} {} {} {} {}  {}",
            c(theme::TEXT, &format!("{:<16.16}", r.workspace)),
            c(&accent, &format!("{:<26.26}", r.branch)),
            c(theme::DIM, &format!("{:>5}", util::age(r.created_at))),
            ahead,
            behind,
            files,
            agent
        );
    }
    Ok(())
}
