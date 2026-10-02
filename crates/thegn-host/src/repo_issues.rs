//! Repo-scoped issue configuration for synchronous host entry points.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use thegn_core::config::{Config, IssuesConfig};

/// Resolve `path` to the main worktree and apply its restrict-only issues overlay.
pub fn for_path(cfg: &Config, path: &Path) -> Result<IssuesConfig> {
    let root = thegn_core::repo::main_worktree(path)
        .with_context(|| format!("{} is not inside a git repository", path.display()))?;
    Ok(cfg.repo_issues(Some(&root)))
}

/// Resolve the current directory when it belongs to a repo; otherwise preserve
/// the global issue configuration used by commands run outside a repository.
pub fn for_cwd(cfg: &Config) -> IssuesConfig {
    let root: Option<PathBuf> = std::env::current_dir()
        .ok()
        .and_then(|cwd| thegn_core::repo::main_worktree(&cwd));
    cfg.repo_issues(root.as_deref())
}

/// Return a config copy whose issue settings are narrowed for the current repo.
pub fn config_for_cwd(cfg: &Config) -> Config {
    let mut scoped = cfg.clone();
    scoped.issues = for_cwd(cfg);
    scoped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use thegn_core::config::{IssueAccount, IssueProviderKind};

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn repo_issue_config_applies_account_and_team_restrictions() {
        let repo = git_repo();
        std::fs::write(
            repo.path().join(".thegn.toml"),
            "[issues]\naccounts = [\"work-linear\"]\n\n[issues.linear]\nteam_id = \"TEAM-PIN\"\n\n[issues.kaneo]\nproject_id = \"PROJECT-PIN\"\n",
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.issues.providers = vec![IssueProviderKind::Linear];
        cfg.issues.issue_accounts = vec![
            IssueAccount {
                name: "work-linear".into(),
                provider: IssueProviderKind::Linear,
                token: "THE720_SYNTHETIC_CANARY".into(),
                ..Default::default()
            },
            IssueAccount {
                name: "other-linear".into(),
                provider: IssueProviderKind::Linear,
                token: "THE720_OTHER_CANARY".into(),
                ..Default::default()
            },
        ];

        let scoped = for_path(&cfg, repo.path()).unwrap();
        assert_eq!(
            scoped
                .issue_accounts
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["work-linear"]
        );
        assert_eq!(scoped.linear.team_id, "TEAM-PIN");
        assert_eq!(scoped.kaneo.project_id, "PROJECT-PIN");
        assert_eq!(
            cfg.issues.issue_accounts.len(),
            2,
            "global settings stay unchanged"
        );
        assert_eq!(
            cfg.repo_issues(None).issue_accounts.len(),
            2,
            "unscoped callers stay global"
        );
    }

    #[test]
    fn explicit_non_repository_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(for_path(&Config::default(), dir.path()).is_err());
    }
}
