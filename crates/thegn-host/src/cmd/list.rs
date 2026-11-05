//! `thegn list` — inventory of managed worktrees, reconciled against git.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::models::WorktreeView;
use thegn_core::remote::GitLoc;
use thegn_core::store::WorkspaceStore;
use thegn_core::{outln, repo, theme, util};

fn is_managed(path: &str, branch: &str, cfg: &Config) -> bool {
    path.starts_with(&cfg.worktrees_dir)
        || path.contains("/.worktrees/")
        || branch.starts_with(&cfg.branch_prefix)
}

/// All thegn-managed worktrees across known repos, with live git status.
pub fn collect(cfg: &Config) -> Result<Vec<WorktreeView>> {
    let db = Db::open()?;

    let meta: HashMap<String, (i64, String)> = db
        .worktrees()?
        .into_iter()
        .map(|w| (w.worktree, (w.created_at, w.agent)))
        .collect();

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
        let name = sessions
            .get(&repo_path)
            .cloned()
            .unwrap_or_else(|| repo::repo_name_from_path(Path::new(&repo_path)));
        let base = match thegn_core::worktree::default_branch_checked(Path::new(&repo_path)) {
            Ok(branch) => Some(branch),
            Err(why) => {
                thegn_core::msg::warn(&format!(
                    "could not read the default branch of {repo_path}: {why}; ahead/behind are not shown for its worktrees"
                ));
                None
            }
        };
        let Some(worktrees) = util::worktree_branches(Path::new(&repo_path)) else {
            continue;
        };
        for (wt_path, wt_branch) in worktrees {
            let wt_branch = wt_branch.as_deref().unwrap_or("");
            if wt_path != repo_path && is_managed(&wt_path, wt_branch, cfg) {
                out.push(view(
                    &wt_path,
                    wt_branch,
                    &name,
                    &repo_path,
                    base.as_deref(),
                    &meta,
                ));
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
    base: Option<&str>,
    meta: &HashMap<String, (i64, String)>,
) -> WorktreeView {
    let p = Path::new(path);
    let exists = p.is_dir();
    let mut dirty = 0;
    let mut ahead = 0;
    let mut behind = 0;
    if exists {
        match dirty_count(p) {
            Ok(count) => dirty = count,
            Err(why) => tracing::warn!(
                target: "thegn::list",
                path = %p.display(),
                "could not read worktree status: {why}"
            ),
        }
        if let Some(base) = base {
            match count_pair(p, base) {
                Ok((behind_count, ahead_count)) => {
                    behind = behind_count;
                    ahead = ahead_count;
                }
                Err(why) => tracing::warn!(
                    target: "thegn::list",
                    path = %p.display(),
                    "could not read worktree ahead/behind counts: {why}"
                ),
            }
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

fn parse_left_right(text: &str) -> Option<(i64, i64)> {
    let mut counts = text.split_whitespace();
    let behind = counts.next()?.parse().ok()?;
    let ahead = counts.next()?.parse().ok()?;
    if counts.next().is_some() {
        return None;
    }
    Some((behind, ahead))
}

fn count_pair(path: &Path, base: &str) -> Result<(i64, i64), String> {
    let output = GitLoc::Local(path.to_path_buf())
        .git_text(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...HEAD"),
        ])
        .map_err(|error| format!("could not read rev-list counts: {error}"))?;
    parse_left_right(&output).ok_or_else(|| format!("invalid rev-list count output: {output:?}"))
}

fn dirty_count(path: &Path) -> Result<i64, String> {
    let output = GitLoc::Local(path.to_path_buf())
        .git_text(&["status", "--porcelain"])
        .map_err(|error| format!("could not read git status: {error}"))?;
    Ok(output.lines().filter(|line| !line.is_empty()).count() as i64)
}

pub fn run(cfg: &Config, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    let rows = collect(cfg)?;
    if json {
        return super::emit_json(&rows);
    }
    if rows.is_empty() {
        outln!("No worktrees yet. Press Alt-W to open a project, Alt-w for a worktree.");
        return Ok(());
    }

    let tty = std::io::stdout().is_terminal();
    let accent = cfg.accent_rgb();
    let c = |on: &str, s: &str| -> String {
        if tty {
            format!("\x1b[38;2;{on}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };

    outln!(
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
        let agent = if r.agent.is_empty() {
            String::new()
        } else if tty {
            let hue = theme::agent_hue(&r.agent);
            format!(
                "{} {}",
                theme::glyph_square(
                    &theme::agent_glyph(&r.agent, crate::caps::agent_glyph_style()),
                    hue,
                ),
                c(hue, &r.agent)
            )
        } else {
            r.agent.clone()
        };
        outln!(
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

#[cfg(test)]
mod tests {
    use super::{count_pair, dirty_count, parse_left_right};
    use std::path::Path;

    #[test]
    fn parse_left_right_reads_behind_then_ahead() {
        assert_eq!(parse_left_right("2\t5"), Some((2, 5)));
        assert_eq!(parse_left_right("0 0"), Some((0, 0)));
        assert_eq!(parse_left_right(""), None);
        assert_eq!(parse_left_right("x y"), None);
        assert_eq!(parse_left_right("1"), None);
    }

    #[expect(clippy::disallowed_methods, reason = "test fixture invokes git")]
    fn git(dir: &Path, args: &[&str]) {
        let output = thegn_core::util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .expect("git fixture command starts");
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn committed_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("fixture"), "first\n").expect("write fixture");
        git(dir, &["add", "fixture"]);
        git(dir, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn dirty_count_and_pair_read_a_real_repo() {
        let dir = tempfile::tempdir().expect("temporary repo");
        committed_repo(dir.path());
        std::fs::write(dir.path().join("untracked"), "content\n").expect("write untracked file");

        assert_eq!(dirty_count(dir.path()), Ok(1));
        assert_eq!(count_pair(dir.path(), "HEAD"), Ok((0, 0)));
    }

    #[test]
    fn count_pair_reports_a_missing_base_instead_of_zero() {
        let dir = tempfile::tempdir().expect("temporary repo");
        committed_repo(dir.path());

        assert!(count_pair(dir.path(), "no-such-branch").is_err());
    }

    #[test]
    fn dirty_count_on_a_non_repository_is_an_error() {
        let dir = tempfile::tempdir_in("/tmp").expect("temporary non-repository");

        assert!(dirty_count(dir.path()).is_err());
    }
}
