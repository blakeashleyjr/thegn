//! Per-repo `[issues]` overlay — the repo-root `.superzej.*` layer that scopes a
//! repo's "My Work" feed (Linear team / Jira project). Kept in a sibling module
//! (rather than the god-file `config.rs`) per the file-size ratchet. See
//! [`crate::config::Config::repo_issues`].

use serde::{Deserialize, Serialize};

use crate::config::{IssueProviderKind, IssuesConfig};

/// Per-repo `[issues]` overlay from a repo-root `.superzej.*` file. Only the
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
