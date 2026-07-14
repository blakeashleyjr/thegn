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
            && self.linear.team_id.is_none()
            && self.linear.workspace_slug.is_none()
            && self.jira.project_key.is_none()
    }
}
