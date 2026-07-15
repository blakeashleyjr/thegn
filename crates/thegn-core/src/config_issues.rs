//! The `[issues]` config family — issue-tracker integration (Linear, GitHub
//! Issues, Jira): the global `[issues]` table, its per-provider sub-tables,
//! and the per-repo `.thegn.*` overlay that scopes a repo's tracker view
//! (Linear team / Jira project). Kept in a sibling module (rather than the
//! god-file `config.rs`) per the file-size ratchet; `config.rs` re-exports
//! everything here. See [`crate::config::Config::repo_issues`].

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};

/// `[issues]` — issue tracker integration (Linear, GitHub Issues, Jira).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IssuesConfig {
    /// Active provider. `"none"` disables the integration. Kept for back-compat;
    /// when `providers` is non-empty it takes precedence over this single value.
    pub provider: IssueProviderKind,
    /// Active providers to aggregate simultaneously, e.g. `["linear", "jira"]`.
    /// When non-empty this wins over the single `provider`; when empty the lone
    /// `provider` is used. Lets a developer track Linear *and* Jira at once.
    #[serde(default)]
    pub providers: Vec<IssueProviderKind>,
    /// Cache TTL (seconds) before a background re-fetch.
    pub ttl_secs: u64,
    /// Maximum issues to fetch and display.
    pub max_issues: usize,
    /// Pre-filter to issues assigned to the authenticated user.
    pub filter_assignee_me: bool,
    /// When a worktree's PR merges, move its linked issue to Done on the tracker.
    /// Off by default — issue lifecycle stays manual unless opted in.
    #[serde(default)]
    pub move_on_merge: bool,
    /// Named accounts to aggregate — multiple per provider (two Linears, a
    /// GitHub + a Jira, …), each with its own token + scope. When non-empty
    /// this is the source of truth; the single sub-tables below are then only a
    /// legacy fallback (used to synthesize accounts when this list is empty).
    #[serde(default)]
    pub issue_accounts: Vec<IssueAccount>,
    pub linear: LinearConfig,
    pub github_issues: GitHubIssuesConfig,
    pub jira: JiraConfig,
}

impl Default for IssuesConfig {
    fn default() -> Self {
        IssuesConfig {
            provider: IssueProviderKind::None,
            providers: Vec::new(),
            ttl_secs: 60,
            max_issues: 100,
            filter_assignee_me: true,
            move_on_merge: false,
            issue_accounts: Vec::new(),
            linear: LinearConfig::default(),
            github_issues: GitHubIssuesConfig::default(),
            jira: JiraConfig::default(),
        }
    }
}

impl IssuesConfig {
    /// The effective set of providers to aggregate, in config order, with `None`
    /// removed and duplicates collapsed. When `providers` is non-empty it wins;
    /// otherwise the single legacy `provider` is used (unless it is `None`).
    pub fn active_providers(&self) -> Vec<IssueProviderKind> {
        let raw: &[IssueProviderKind] = if self.providers.is_empty() {
            std::slice::from_ref(&self.provider)
        } else {
            &self.providers
        };
        let mut out: Vec<IssueProviderKind> = Vec::new();
        for &p in raw {
            if p != IssueProviderKind::None && !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    /// The effective set of issue accounts to aggregate, in config order.
    ///
    /// When `issue_accounts` is non-empty it wins — every `enabled` entry is
    /// returned (a `None`-provider entry is dropped). When it is empty this is
    /// the **back-compat path**: one account is synthesized per
    /// [`active_providers`](Self::active_providers) from the legacy single
    /// sub-tables, so a config with no `[[issue_accounts]]` fetches exactly as
    /// it did before named accounts existed.
    pub fn active_accounts(&self) -> Vec<IssueAccount> {
        if !self.issue_accounts.is_empty() {
            return self
                .issue_accounts
                .iter()
                .filter(|a| a.enabled && a.provider != IssueProviderKind::None)
                .cloned()
                .collect();
        }
        self.active_providers()
            .into_iter()
            .map(|p| self.synth_legacy_account(p))
            .collect()
    }

    /// Synthesize a named account for `provider` from the legacy single
    /// sub-tables (the back-compat bridge for `active_accounts`).
    fn synth_legacy_account(&self, provider: IssueProviderKind) -> IssueAccount {
        let mut a = IssueAccount {
            name: provider.as_str().to_string(),
            provider,
            enabled: true,
            ..IssueAccount::default()
        };
        match provider {
            IssueProviderKind::Linear => {
                a.token = self.linear.api_key.clone();
                a.team_id = self.linear.team_id.clone();
                a.workspace_slug = self.linear.workspace_slug.clone();
            }
            IssueProviderKind::Jira => {
                a.token = self.jira.api_token.clone();
                a.base_url = self.jira.base_url.clone();
                a.email = self.jira.email.clone();
                a.project_key = self.jira.project_key.clone();
            }
            IssueProviderKind::Github => {
                a.extra_flags = self.github_issues.extra_flags.clone();
            }
            IssueProviderKind::None => {}
        }
        a
    }
}

/// A `[[issue_accounts]]` entry — one named tracker login. Multiple entries may
/// share a `provider` (two Linear workspaces, a personal + work GitHub, …);
/// each carries its own token and scope, and all `enabled` entries aggregate
/// into the unified "My Work" feed. Mirrors the coding-agent `[[accounts]]`
/// precedent ([`crate::account::Account`]). Only the fields relevant to the
/// `provider` are read (the rest stay at their empty default).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IssueAccount {
    /// Stable id for this account, e.g. `"work-linear"`. Also the cache key.
    pub name: String,
    /// Which tracker backend this account talks to.
    pub provider: IssueProviderKind,
    /// Aggregate this account? Disabled entries are kept in config but skipped.
    pub enabled: bool,
    /// Provider token. Use a secret ref or `"env:VAR"` (resolved at fetch time).
    /// Linear: API key. Jira: API token. GitHub: unused (`gh` handles auth).
    pub token: String,
    /// Linear: restrict to a single team id (`""` = all teams).
    pub team_id: String,
    /// Linear: workspace slug (used for URLs; inferred if empty).
    pub workspace_slug: String,
    /// Jira: instance base URL, e.g. `"https://myorg.atlassian.net"`.
    pub base_url: String,
    /// Jira: user email (Basic-auth identity).
    pub email: String,
    /// Jira: restrict to a single project key (`""` = all).
    pub project_key: String,
    /// GitHub Issues: extra `gh issue list` flags.
    pub extra_flags: Vec<String>,
    /// Optional named-forge ref (see `[[forges]]`) for GitHub accounts; `""`
    /// uses the default forge.
    pub forge: String,
}

impl Default for IssueAccount {
    fn default() -> Self {
        // `enabled` defaults to true so a `[[issue_accounts]]` entry that omits
        // it still aggregates (serde container `default` fills missing fields
        // from this impl).
        IssueAccount {
            name: String::new(),
            provider: IssueProviderKind::None,
            enabled: true,
            token: String::new(),
            team_id: String::new(),
            workspace_slug: String::new(),
            base_url: String::new(),
            email: String::new(),
            project_key: String::new(),
            extra_flags: Vec::new(),
            forge: String::new(),
        }
    }
}

config_enum! {
    /// Which issue tracker backend is active.
    pub enum IssueProviderKind : "issue provider" {
        None    = "none",
        Linear  = "linear",
        Github  = "github",
        Jira    = "jira",
    } default = None;
}

/// `[issues.linear]` — Linear.app configuration.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LinearConfig {
    /// API key. Use `"env:LINEAR_API_KEY"` to read from the environment.
    pub api_key: String,
    /// Restrict to a single team id. `""` = all teams.
    pub team_id: String,
    /// Optional workspace slug (used for URLs; inferred if empty).
    pub workspace_slug: String,
}

impl Default for LinearConfig {
    fn default() -> Self {
        LinearConfig {
            api_key: "env:LINEAR_API_KEY".into(),
            team_id: String::new(),
            workspace_slug: String::new(),
        }
    }
}

/// `[issues.github_issues]` — GitHub Issues configuration.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema, Default)]
#[serde(default)]
pub struct GitHubIssuesConfig {
    /// Additional `gh issue list` flags, e.g. `--assignee @me --label bug`.
    pub extra_flags: Vec<String>,
}

/// `[issues.jira]` — Jira Cloud/Server configuration.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct JiraConfig {
    /// Jira instance base URL, e.g. `"https://myorg.atlassian.net"`.
    pub base_url: String,
    /// Jira user email.
    pub email: String,
    /// API token. Use `"env:JIRA_API_TOKEN"` to read from the environment.
    pub api_token: String,
    /// Restrict to a single project key, e.g. `"PROJ"`. `""` = all projects.
    pub project_key: String,
}

impl Default for JiraConfig {
    fn default() -> Self {
        JiraConfig {
            base_url: String::new(),
            email: String::new(),
            api_token: "env:JIRA_API_TOKEN".into(),
            project_key: String::new(),
        }
    }
}

/// Per-repo `[issues]` overlay from a repo-root `.thegn.*` file. Only the
/// present keys override the global `[issues]`, letting a repo pin the Linear
/// team / Jira project that scopes its "My Work" feed (GitHub is auto-scoped to
/// the repo's remote and needs no config). Same Option-field shape as
/// `SandboxOverlay`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IssuesOverlay {
    /// Restrict the providers aggregated for this repo (empty vec = none).
    pub providers: Option<Vec<IssueProviderKind>>,
    /// Restrict the explicit `[[issue_accounts]]` aggregated for this repo, by
    /// account name (empty vec = none). The legacy synthesized path is scoped
    /// via `providers` instead.
    pub accounts: Option<Vec<String>>,
    pub linear: LinearOverlay,
    pub jira: JiraOverlay,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LinearOverlay {
    /// Restrict to a single Linear team id for this repo (`""` = all teams).
    pub team_id: Option<String>,
    /// Workspace slug used for issue URLs.
    pub workspace_slug: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct JiraOverlay {
    /// Restrict to a single Jira project key for this repo (`""` = all).
    pub project_key: Option<String>,
}

impl IssuesOverlay {
    /// Field-merge present keys into a base [`IssuesConfig`] (absent inherit).
    pub(crate) fn apply(self, base: &mut IssuesConfig) {
        if let Some(p) = self.providers {
            base.providers = p;
        }
        if let Some(names) = self.accounts {
            base.issue_accounts.retain(|a| names.contains(&a.name));
        }
        if let Some(t) = self.linear.team_id {
            base.linear.team_id = t;
        }
        if let Some(w) = self.linear.workspace_slug {
            base.linear.workspace_slug = w;
        }
        if let Some(k) = self.jira.project_key {
            base.jira.project_key = k;
        }
    }

    /// Whether the overlay carries no overrides (skip applying it).
    pub(crate) fn is_empty(&self) -> bool {
        self.providers.is_none()
            && self.accounts.is_none()
            && self.linear.team_id.is_none()
            && self.linear.workspace_slug.is_none()
            && self.jira.project_key.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_accounts_empty_by_default() {
        assert!(IssuesConfig::default().active_accounts().is_empty());
    }

    #[test]
    fn active_accounts_synthesizes_from_legacy_single_provider() {
        // A legacy config (no `[[issue_accounts]]`) synthesizes one account
        // per active provider, carrying the sub-table token + scope.
        let cfg = IssuesConfig {
            provider: IssueProviderKind::Linear,
            linear: LinearConfig {
                api_key: "secret".into(),
                team_id: "TEAM".into(),
                workspace_slug: "ws".into(),
            },
            ..Default::default()
        };
        let accts = cfg.active_accounts();
        assert_eq!(accts.len(), 1);
        let a = &accts[0];
        assert_eq!(a.name, "linear");
        assert_eq!(a.provider, IssueProviderKind::Linear);
        assert!(a.enabled);
        assert_eq!(a.token, "secret");
        assert_eq!(a.team_id, "TEAM");
        assert_eq!(a.workspace_slug, "ws");
    }

    #[test]
    fn active_accounts_synthesizes_from_legacy_providers_list() {
        let cfg = IssuesConfig {
            providers: vec![IssueProviderKind::Linear, IssueProviderKind::Jira],
            jira: JiraConfig {
                base_url: "https://x".into(),
                email: "me@x".into(),
                api_token: "jt".into(),
                project_key: "PROJ".into(),
            },
            ..Default::default()
        };
        // Two synthesized accounts, in provider order.
        assert_eq!(cfg.active_accounts().len(), 2);
        assert_eq!(
            cfg.active_accounts()
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>(),
            vec!["linear".to_string(), "jira".to_string()]
        );
        let jira = cfg
            .active_accounts()
            .into_iter()
            .find(|a| a.provider == IssueProviderKind::Jira)
            .unwrap();
        assert_eq!(jira.token, "jt");
        assert_eq!(jira.base_url, "https://x");
        assert_eq!(jira.project_key, "PROJ");
    }

    #[test]
    fn explicit_accounts_win_and_filter_disabled() {
        let cfg = IssuesConfig {
            // Legacy provider is ignored once explicit accounts exist.
            provider: IssueProviderKind::Github,
            issue_accounts: vec![
                IssueAccount {
                    name: "work".into(),
                    provider: IssueProviderKind::Linear,
                    ..Default::default()
                },
                IssueAccount {
                    name: "off".into(),
                    provider: IssueProviderKind::Jira,
                    enabled: false,
                    ..Default::default()
                },
                IssueAccount {
                    name: "bogus".into(),
                    provider: IssueProviderKind::None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let accts = cfg.active_accounts();
        // Disabled + None-provider entries are dropped; the legacy provider does
        // not leak in.
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0].name, "work");
    }

    #[test]
    fn overlay_restricts_explicit_accounts_by_name() {
        let mut base = IssuesConfig {
            issue_accounts: vec![
                IssueAccount {
                    name: "a".into(),
                    provider: IssueProviderKind::Linear,
                    ..Default::default()
                },
                IssueAccount {
                    name: "b".into(),
                    provider: IssueProviderKind::Linear,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let overlay = IssuesOverlay {
            accounts: Some(vec!["b".into()]),
            ..Default::default()
        };
        assert!(!overlay.is_empty());
        overlay.apply(&mut base);
        assert_eq!(base.active_accounts().len(), 1);
        assert_eq!(base.active_accounts()[0].name, "b");
    }
}
