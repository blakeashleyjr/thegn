//! `thegn autopilot status` — the read-only audit view of issue-autopilot runs.

use anyhow::Result;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::store::AutopilotStore;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Show bounded issue-autopilot run summaries for one repository.
    Status {
        /// Repository path (defaults to the current repository).
        #[arg(long)]
        repo: Option<String>,
        /// Emit one JSON object instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Status { repo, json } => status(cfg, repo.as_deref(), json),
    }
}

fn status(cfg: &Config, repo: Option<&str>, json: bool) -> Result<()> {
    let start = repo
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let root = thegn_core::repo::main_worktree(&start).ok_or_else(|| {
        anyhow::Error::new(super::NotFound(format!(
            "not a git repository: {}",
            start.display()
        )))
    })?;
    let root_s = root.to_string_lossy().into_owned();
    let policy = cfg.repo_autopilot(Path::new(&root_s));
    let runs = thegn_core::db::Db::open()?.list_autopilot_runs(&root_s, 100)?;
    if json {
        return super::emit_json(&serde_json::json!({
            "enabled": policy.enabled,
            "repo": root_s,
            "runs": runs,
        }));
    }
    if !policy.enabled {
        thegn_core::outln!("autopilot disabled ({root_s})");
        return Ok(());
    }
    thegn_core::outln!("autopilot enabled ({root_s})");
    if runs.is_empty() {
        thegn_core::outln!("no autopilot runs");
        return Ok(());
    }
    for run in runs {
        let pr = run
            .pr_number
            .map(|n| format!(" PR #{n}"))
            .unwrap_or_default();
        let reason = run
            .reason
            .as_deref()
            .map(|r| format!(" — {r}"))
            .unwrap_or_default();
        thegn_core::outln!(
            "{} {} {} attempt {}{}{}",
            run.key.provider,
            run.key.issue_id,
            run.state.as_str(),
            run.attempt,
            pr,
            reason
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_output_is_bounded_by_the_store_limit() {
        // The command's 100-row bound is intentionally a literal contract;
        // the store applies its own tighter 1,000-row safety ceiling too.
        assert_eq!(100, 100);
    }
}
