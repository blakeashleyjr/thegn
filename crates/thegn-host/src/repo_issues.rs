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

/// Resolve `dir` (when given and inside a repo) to its main worktree and apply
/// that repo's overlay; otherwise preserve the global issue configuration.
pub fn for_dir(cfg: &Config, dir: Option<&Path>) -> IssuesConfig {
    let root: Option<PathBuf> = dir.and_then(thegn_core::repo::main_worktree);
    cfg.repo_issues(root.as_deref())
}

/// Return a config copy whose issue settings are narrowed for `dir`'s repo.
pub fn config_for_dir(cfg: &Config, dir: Option<&Path>) -> Config {
    let mut scoped = cfg.clone();
    scoped.issues = for_dir(cfg, dir);
    scoped
}

/// Which issues config the merge-refresh `move_on_merge` router is built from.
///
/// `local_cwd` is the worktree path when the location is plain local; for a
/// remote / provider location (or when the repo root cannot be resolved) the
/// GLOBAL config is used unchanged ("omitted context = global"), exactly as
/// before repo scoping existed. The reason is logged at debug.
pub fn for_merge_refresh(cfg: &Config, local_cwd: Option<&Path>) -> IssuesConfig {
    let Some(cwd) = local_cwd else {
        tracing::debug!(target: "thegn::issues", "move_on_merge: non-local location, using global issues config");
        return cfg.issues.clone();
    };
    match for_path(cfg, cwd) {
        Ok(scoped) => scoped,
        Err(e) => {
            tracing::debug!(target: "thegn::issues", error = %e, "move_on_merge: repo unresolved, using global issues config");
            cfg.issues.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn repo_issue_config_applies_account_and_team_restrictions() {
        let repo = overlay_repo();
        let cfg = two_account_config();

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

    #[test]
    fn for_dir_scopes_inside_a_repo_and_stays_global_outside() {
        let repo = overlay_repo();
        let cfg = two_account_config();
        let inside = for_dir(&cfg, Some(repo.path()));
        assert_eq!(inside.issue_accounts.len(), 1);
        assert_eq!(inside.linear.team_id, "TEAM-PIN");
        let outside = tempfile::tempdir().unwrap();
        for dir in [Some(outside.path()), None] {
            let global = for_dir(&cfg, dir);
            assert_eq!(global.issue_accounts.len(), 2);
            assert_eq!(global.linear.team_id, cfg.issues.linear.team_id);
        }
        assert_eq!(
            config_for_dir(&cfg, Some(repo.path()))
                .issues
                .kaneo
                .project_id,
            "PROJECT-PIN"
        );
        assert_eq!(
            config_for_dir(&cfg, None).issues.kaneo.project_id,
            cfg.issues.kaneo.project_id
        );
    }

    #[test]
    fn merge_refresh_uses_overlay_for_local_repo() {
        let repo = overlay_repo();
        let cfg = two_account_config();
        let got = for_merge_refresh(&cfg, Some(repo.path()));
        assert_eq!(got.issue_accounts.len(), 1);
        assert_eq!(got.linear.team_id, "TEAM-PIN");
    }

    #[test]
    fn merge_refresh_falls_back_to_global_without_a_resolvable_repo() {
        let cfg = two_account_config();
        // Non-local / provider location: no local path at all.
        let remote = for_merge_refresh(&cfg, None);
        assert_eq!(remote.issue_accounts.len(), 2);
        assert_eq!(remote.linear.team_id, cfg.issues.linear.team_id);
        // main_worktree fails (not a repo).
        let outside = tempfile::tempdir().unwrap();
        let unresolved = for_merge_refresh(&cfg, Some(outside.path()));
        assert_eq!(unresolved.issue_accounts.len(), 2);
        assert_eq!(unresolved.linear.team_id, cfg.issues.linear.team_id);
    }
}

/// Shared fixtures for the repo-scoping tests (CLI, kaneo, daemon).
#[cfg(test)]
pub(crate) mod test_support {
    use thegn_core::config::{Config, IssueAccount, IssueProviderKind};

    /// A real git repo whose `.thegn.toml` restricts accounts and pins a
    /// Linear team and a Kaneo project.
    pub(crate) fn overlay_repo() -> tempfile::TempDir {
        let dir = git_repo();
        std::fs::write(
            dir.path().join(".thegn.toml"),
            "[issues]\naccounts = [\"work-linear\"]\n\n[issues.linear]\nteam_id = \"TEAM-PIN\"\n\n[issues.kaneo]\nproject_id = \"PROJECT-PIN\"\n",
        )
        .unwrap();
        dir
    }

    #[expect(clippy::disallowed_methods)] // deterministic real-Git fixture, test only
    pub(crate) fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let out = thegn_core::util::git_cmd(dir.path())
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(out.status.success());
        dir
    }

    /// Global config with two synthetic-canary Linear accounts and a global
    /// Kaneo project id that differs from the overlay's pin.
    pub(crate) fn two_account_config() -> Config {
        let mut cfg = Config::default();
        cfg.issues.providers = vec![IssueProviderKind::Linear];
        cfg.issues.kaneo.project_id = "PROJECT-GLOBAL".into();
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
        cfg
    }
}
