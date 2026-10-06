//! The `[issues]` config family — issue-tracker integration (Linear, GitHub
//! Issues, Jira, Kaneo): the global `[issues]` table, its per-provider
//! sub-tables, and the per-repo `.thegn.*` overlay that scopes a repo's tracker
//! view (Linear team / Jira project / Kaneo project). Kept in a sibling module
//! (rather than the god-file `config.rs`) to keep it flat; `config.rs`
//! re-exports everything here. See [`crate::config::Config::repo_issues`].

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
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
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
    pub kaneo: KaneoConfig,
    /// Set by a repo `[issues]` overlay that supplies an explicit `accounts`
    /// restriction (including `accounts = []`): once a repo has restricted its
    /// accounts, the legacy single-provider synthesis in `active_accounts` is
    /// suppressed so an empty restriction means *none*, not "resurrect legacy".
    /// Runtime-only; never read from / written to a config file.
    #[serde(skip)]
    pub accounts_restricted: bool,
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
            kaneo: KaneoConfig::default(),
            accounts_restricted: false,
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
        // A repo that explicitly restricted its accounts (`accounts = [...]`,
        // including `accounts = []`) opted out of the legacy synthesis fallback;
        // an empty restriction must mean *no accounts*, not "resurrect the legacy
        // single-provider account (with its token)".
        if self.accounts_restricted {
            return Vec::new();
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
            IssueProviderKind::Kaneo => {
                a.token = self.kaneo.api_key.clone();
                a.base_url = self.kaneo.base_url.clone();
                a.workspace_id = self.kaneo.workspace_id.clone();
                a.project_id = self.kaneo.project_id.clone();
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
    /// Linear: API key. Jira: API token. Kaneo: API key (or empty to use a
    /// stored device-flow token). GitHub: unused (`gh` handles auth).
    pub token: String,
    /// Linear: restrict to a single team id (`""` = all teams).
    pub team_id: String,
    /// Linear: workspace slug. Only affects issue URLs (inferred if empty); it
    /// is not a scope or access boundary.
    pub workspace_slug: String,
    /// Jira / Kaneo: instance base URL, e.g. `"https://myorg.atlassian.net"` or
    /// `"https://kaneo.example.com"`.
    pub base_url: String,
    /// Jira: user email (Basic-auth identity).
    pub email: String,
    /// Jira: restrict to a single project key (`""` = all).
    pub project_key: String,
    /// Kaneo: restrict to a single workspace id (`""` = the first workspace).
    pub workspace_id: String,
    /// Kaneo: restrict to a single project id (`""` = all projects in the
    /// workspace). Also the default project for `create`.
    pub project_id: String,
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
            workspace_id: String::new(),
            project_id: String::new(),
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
        Kaneo   = "kaneo",
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
    /// Optional workspace slug. Only affects issue URLs (inferred if empty); it
    /// is not a scope or access boundary.
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

/// `[issues.kaneo]` — Kaneo (self-hosted, open-source PM) configuration.
///
/// Kaneo's REST API is served under `{base_url}/api` and accepts a static API
/// key (better-auth apiKey plugin) as an `Authorization: Bearer` token. When
/// `api_key` is empty, the backend falls back to a token stored by
/// `thegn kaneo login` (device-flow) for this `base_url`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct KaneoConfig {
    /// Instance base URL, e.g. `"https://kaneo.example.com"` (no trailing
    /// `/api`). Required; empty disables the Kaneo backend.
    pub base_url: String,
    /// API key. Use `"env:KANEO_API_KEY"` to read from the environment. Empty
    /// means "use the device-flow token stored for `base_url`".
    pub api_key: String,
    /// Restrict to a single workspace id. `""` = the first workspace.
    pub workspace_id: String,
    /// Restrict to a single project id. `""` = all projects in the workspace.
    /// Also the default project for issue creation.
    pub project_id: String,
}

impl Default for KaneoConfig {
    fn default() -> Self {
        KaneoConfig {
            base_url: String::new(),
            api_key: "env:KANEO_API_KEY".into(),
            workspace_id: String::new(),
            project_id: String::new(),
        }
    }
}

/// Per-repo `[issues]` overlay from a repo-root `.thegn.*` file. Only the
/// present keys override the global `[issues]`, letting a repo pin the Linear
/// team / Jira project that scopes its "My Work" feed (GitHub is auto-scoped to
/// the repo's remote and needs no config). Same Option-field shape as
/// `SandboxOverlay`.
///
/// The overlay is restrict-only: a repo may narrow an UNPINNED global scope
/// (including choosing the default destination for user-initiated creates) but
/// never widen it — providers are intersected with the globally enabled set and
/// a non-empty global pin is a ceiling. Pins apply to the legacy
/// single-provider sub-tables and, in accounts mode (`[[issue_accounts]]`), to
/// each matching account with the account's own pin as the ceiling.
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
    pub kaneo: KaneoOverlay,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct KaneoOverlay {
    /// Restrict to a single Kaneo workspace id for this repo.
    pub workspace_id: Option<String>,
    /// Restrict to a single Kaneo project id for this repo (`""` = all).
    pub project_id: Option<String>,
}

/// A repo pin may only narrow a global pin: an empty global pin means "all", so
/// any repo pin is within it; a non-empty global pin is a ceiling the repo can
/// only restate. Anything else is refused (the global pin stays) and reported.
fn narrow_pin(field: &str, global: &mut String, repo: String, refused: &mut Vec<String>) {
    if global.is_empty() || *global == repo {
        *global = repo;
    } else {
        refused.push(format!("{field} pin is outside the global scope"));
    }
}

/// Kaneo workspace + project are ONE nested ceiling: a project may live in
/// another workspace and the config layer cannot verify membership.
fn narrow_kaneo(
    g_ws: &mut String,
    g_proj: &mut String,
    ws_pin: Option<String>,
    proj_pin: Option<String>,
    refused: &mut Vec<String>,
    who: &str,
) {
    let (orig_ws, orig_proj) = (g_ws.clone(), g_proj.clone());
    if let Some(w) = ws_pin {
        if orig_ws.is_empty() && !orig_proj.is_empty() && w != orig_ws {
            refused.push(format!(
                "{who}kaneo.workspace_id pin is outside the global scope"
            ));
        } else {
            narrow_pin(&format!("{who}kaneo.workspace_id"), g_ws, w, refused);
        }
    }
    if let Some(p) = proj_pin {
        if orig_proj.is_empty() && !orig_ws.is_empty() && !p.is_empty() {
            refused.push(format!(
                "{who}kaneo.project_id pin is outside the global scope"
            ));
        } else {
            narrow_pin(&format!("{who}kaneo.project_id"), g_proj, p, refused);
        }
    }
}

/// Bounded per-process set of already-logged `(repo root, message)` refusals.
static LOGGED_REFUSALS: std::sync::Mutex<Vec<(std::path::PathBuf, String)>> =
    std::sync::Mutex::new(Vec::new());
const LOGGED_REFUSALS_CAP: usize = 256;

/// True the first time `(root, msg)` is seen in this process (bounded: the set
/// is cleared when it reaches the cap, so a flood can only cause re-logging).
fn first_refusal(root: &std::path::Path, msg: &str) -> bool {
    let Ok(mut seen) = LOGGED_REFUSALS.lock() else {
        return true;
    };
    if seen.iter().any(|(r, m)| r == root && m == msg) {
        return false;
    }
    if seen.len() >= LOGGED_REFUSALS_CAP {
        seen.clear();
    }
    seen.push((root.to_path_buf(), msg.to_string()));
    true
}

/// Warn about refused overlay widening, once per `(repo root, message)`.
pub(crate) fn warn_refusals_once(root: &std::path::Path, refused: &[String]) {
    for why in refused {
        if first_refusal(root, why) {
            tracing::warn!(
                repo = %root.display(),
                "repo [issues] overlay refused (restrict-only): {why}"
            );
        }
    }
}

impl IssuesOverlay {
    /// Field-merge present keys into a base [`IssuesConfig`] (absent inherit).
    ///
    /// The overlay is RESTRICT-ONLY: providers are intersected with the globally
    /// enabled set and pins may only narrow a global pin. Returns a diagnostic
    /// per refused widening attempt (the value is never applied).
    pub(crate) fn apply(self, base: &mut IssuesConfig) -> Vec<String> {
        let mut refused = Vec::new();
        if let Some(p) = self.providers {
            // "Enabled" is the legacy provider set plus the providers of the
            // active explicit accounts (accounts mode).
            // In accounts mode (`issue_accounts` non-empty) only the accounts'
            // providers count: legacy providers there have no active account.
            let accounts_mode = !base.issue_accounts.is_empty();
            let mut enabled = if accounts_mode {
                Vec::new()
            } else {
                base.active_providers()
            };
            for a in base.active_accounts() {
                if !enabled.contains(&a.provider) {
                    enabled.push(a.provider);
                }
            }
            let mut kept: Vec<IssueProviderKind> = Vec::new();
            for k in p {
                if enabled.contains(&k) {
                    if !kept.contains(&k) {
                        kept.push(k);
                    }
                } else if k != IssueProviderKind::None {
                    refused.push(format!("provider {} is not enabled globally", k.as_str()));
                }
            }
            // An explicit restriction is authoritative: also clear the legacy
            // single `provider` so `providers = []` resolves to *none* instead of
            // falling back to the legacy provider in `active_providers`.
            base.provider = IssueProviderKind::None;
            // In accounts mode the intersected set also filters the accounts.
            base.issue_accounts.retain(|a| kept.contains(&a.provider));
            if accounts_mode {
                // An emptied list must mean NO accounts, never a re-synthesis
                // of legacy accounts (and their sub-table tokens).
                base.accounts_restricted = true;
            }
            base.providers = kept;
        }
        if let Some(names) = self.accounts {
            base.issue_accounts.retain(|a| names.contains(&a.name));
            // Mark the accounts restricted so `active_accounts` skips the legacy
            // synthesis fallback (an empty/typo'd restriction ⇒ zero accounts).
            base.accounts_restricted = true;
        }
        let accounts_mode = !base.issue_accounts.is_empty();
        // Legacy sub-table pins. In accounts mode those tables are not read
        // (each account carries its own scope), so their diagnostics are moot.
        let mut legacy_refused = Vec::new();
        {
            let sink = if accounts_mode {
                &mut legacy_refused
            } else {
                &mut refused
            };
            if let Some(t) = self.linear.team_id.clone() {
                narrow_pin("linear.team_id", &mut base.linear.team_id, t, sink);
            }
            if let Some(k) = self.jira.project_key.clone() {
                narrow_pin("jira.project_key", &mut base.jira.project_key, k, sink);
            }
            narrow_kaneo(
                &mut base.kaneo.workspace_id,
                &mut base.kaneo.project_id,
                self.kaneo.workspace_id.clone(),
                self.kaneo.project_id.clone(),
                sink,
                "",
            );
        }
        if let Some(w) = self.linear.workspace_slug {
            base.linear.workspace_slug = w;
        }
        // Accounts mode: the same restrict-only rule per matching account, the
        // account's own pin being the ceiling.
        for a in &mut base.issue_accounts {
            let who = format!("account {}: ", a.name);
            match a.provider {
                IssueProviderKind::Linear => {
                    if let Some(t) = self.linear.team_id.clone() {
                        narrow_pin(
                            &format!("{who}linear.team_id"),
                            &mut a.team_id,
                            t,
                            &mut refused,
                        );
                    }
                }
                IssueProviderKind::Jira => {
                    if let Some(k) = self.jira.project_key.clone() {
                        narrow_pin(
                            &format!("{who}jira.project_key"),
                            &mut a.project_key,
                            k,
                            &mut refused,
                        );
                    }
                }
                IssueProviderKind::Kaneo => narrow_kaneo(
                    &mut a.workspace_id,
                    &mut a.project_id,
                    self.kaneo.workspace_id.clone(),
                    self.kaneo.project_id.clone(),
                    &mut refused,
                    &who,
                ),
                _ => {}
            }
        }
        refused
    }

    /// Whether the overlay carries no overrides (skip applying it).
    pub(crate) fn is_empty(&self) -> bool {
        self.providers.is_none()
            && self.accounts.is_none()
            && self.linear.team_id.is_none()
            && self.linear.workspace_slug.is_none()
            && self.jira.project_key.is_none()
            && self.kaneo.workspace_id.is_none()
            && self.kaneo.project_id.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_providers_cannot_activate_unused_legacy_credential() {
        let mut base = IssuesConfig {
            jira: JiraConfig {
                api_token: "LEGACY_JIRA_CANARY".into(),
                ..Default::default()
            },
            provider: IssueProviderKind::Jira,
            issue_accounts: vec![IssueAccount {
                name: "lin".into(),
                provider: IssueProviderKind::Linear,
                token: "LIN_CANARY".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let refused = IssuesOverlay {
            providers: Some(vec![IssueProviderKind::Jira]),
            ..Default::default()
        }
        .apply(&mut base);
        assert_eq!(refused.len(), 1);
        assert!(base.active_accounts().is_empty());
    }

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

    #[test]
    fn overlay_empty_providers_means_none_not_legacy() {
        // Global legacy config: a single provider with a token.
        let mut base = IssuesConfig {
            provider: IssueProviderKind::Linear,
            linear: LinearConfig {
                api_key: "work-token".into(),
                team_id: "TEAM".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        // A repo opts out with `providers = []` (documented "empty vec = none").
        IssuesOverlay {
            providers: Some(vec![]),
            ..Default::default()
        }
        .apply(&mut base);
        // The legacy provider must NOT be resurrected: zero providers, zero
        // accounts (no Linear query with the work token for this repo).
        assert!(
            base.active_providers().is_empty(),
            "providers = [] must resolve to none, not the legacy provider"
        );
        assert!(
            base.active_accounts().is_empty(),
            "no legacy account may be synthesized for an opted-out repo"
        );
    }

    #[test]
    fn overlay_empty_accounts_means_none_not_legacy_synthesis() {
        // Legacy config (no explicit [[issue_accounts]]) with a Jira token.
        let mut base = IssuesConfig {
            provider: IssueProviderKind::Jira,
            jira: JiraConfig {
                base_url: "https://work".into(),
                email: "me@work".into(),
                api_token: "jira-token".into(),
                project_key: "PROJ".into(),
            },
            ..Default::default()
        };
        // A repo restricts accounts to none (`accounts = []`).
        IssuesOverlay {
            accounts: Some(vec![]),
            ..Default::default()
        }
        .apply(&mut base);
        // Legacy synthesis must be suppressed: the work Jira account (with its
        // token) must NOT be resurrected for the opted-out repo.
        assert!(
            base.active_accounts().is_empty(),
            "accounts = [] must suppress legacy account synthesis"
        );
    }

    #[test]
    fn active_accounts_synthesizes_kaneo_from_legacy_sub_table() {
        // A legacy Kaneo config (no `[[issue_accounts]]`) synthesizes one account
        // carrying the sub-table api_key + base_url + workspace/project scope.
        let cfg = IssuesConfig {
            provider: IssueProviderKind::Kaneo,
            kaneo: KaneoConfig {
                base_url: "https://kaneo.example.com".into(),
                api_key: "k-secret".into(),
                workspace_id: "ws-1".into(),
                project_id: "proj-1".into(),
            },
            ..Default::default()
        };
        let accts = cfg.active_accounts();
        assert_eq!(accts.len(), 1);
        let a = &accts[0];
        assert_eq!(a.name, "kaneo");
        assert_eq!(a.provider, IssueProviderKind::Kaneo);
        assert!(a.enabled);
        assert_eq!(a.token, "k-secret");
        assert_eq!(a.base_url, "https://kaneo.example.com");
        assert_eq!(a.workspace_id, "ws-1");
        assert_eq!(a.project_id, "proj-1");
    }

    #[test]
    fn kaneo_provider_parses_and_round_trips() {
        assert_eq!(
            IssueProviderKind::from_str_validated("kaneo").unwrap(),
            IssueProviderKind::Kaneo
        );
        assert_eq!(IssueProviderKind::Kaneo.as_str(), "kaneo");
    }

    #[test]
    fn kaneo_overlay_pins_workspace_and_project() {
        let mut base = IssuesConfig {
            provider: IssueProviderKind::Kaneo,
            kaneo: KaneoConfig {
                base_url: "https://kaneo.example.com".into(),
                api_key: "tok".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = IssuesOverlay {
            kaneo: KaneoOverlay {
                workspace_id: Some("ws-9".into()),
                project_id: Some("proj-9".into()),
            },
            ..Default::default()
        };
        assert!(!overlay.is_empty());
        overlay.apply(&mut base);
        assert_eq!(base.kaneo.workspace_id, "ws-9");
        assert_eq!(base.kaneo.project_id, "proj-9");
        // The synthesized account carries the pinned scope.
        let a = &base.active_accounts()[0];
        assert_eq!(a.workspace_id, "ws-9");
        assert_eq!(a.project_id, "proj-9");
    }

    #[test]
    fn overlay_typod_account_name_restricts_to_none() {
        // A restriction naming a non-existent account empties issue_accounts and
        // must NOT fall back to legacy synthesis.
        let mut base = IssuesConfig {
            provider: IssueProviderKind::Linear,
            linear: LinearConfig {
                api_key: "tok".into(),
                ..Default::default()
            },
            issue_accounts: vec![IssueAccount {
                name: "real".into(),
                provider: IssueProviderKind::Linear,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        IssuesOverlay {
            accounts: Some(vec!["typo".into()]),
            ..Default::default()
        }
        .apply(&mut base);
        assert!(
            base.active_accounts().is_empty(),
            "a typo'd account restriction must yield zero accounts, not legacy"
        );
    }

    fn acct(name: &str, provider: IssueProviderKind) -> IssueAccount {
        IssueAccount {
            name: name.into(),
            provider,
            token: "canary-token".into(),
            ..Default::default()
        }
    }

    #[test]
    fn overlay_pins_narrow_each_matching_account() {
        let mut lin = acct("l", IssueProviderKind::Linear);
        let mut lin_pinned = acct("lp", IssueProviderKind::Linear);
        lin_pinned.team_id = "T0".into();
        let jira = acct("j", IssueProviderKind::Jira);
        let kan = acct("k", IssueProviderKind::Kaneo);
        lin.team_id = String::new();
        let mut base = IssuesConfig {
            issue_accounts: vec![lin, lin_pinned, jira, kan],
            ..Default::default()
        };
        let refused = IssuesOverlay {
            linear: LinearOverlay {
                team_id: Some("T1".into()),
                workspace_slug: None,
            },
            jira: JiraOverlay {
                project_key: Some("PK".into()),
            },
            kaneo: KaneoOverlay {
                workspace_id: Some("w".into()),
                project_id: Some("p".into()),
            },
            ..Default::default()
        }
        .apply(&mut base);
        let by = |n: &str| {
            base.issue_accounts
                .iter()
                .find(|a| a.name == n)
                .unwrap()
                .clone()
        };
        assert_eq!(by("l").team_id, "T1");
        // The account's own pin is a ceiling: widening/replacing is refused.
        assert_eq!(by("lp").team_id, "T0");
        assert_eq!(by("j").project_key, "PK");
        assert_eq!(
            (by("k").workspace_id, by("k").project_id),
            ("w".into(), "p".into())
        );
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("account lp"), "{refused:?}");
    }

    #[test]
    fn overlay_kaneo_account_joint_ceiling_and_no_widening() {
        let mut k = acct("k", IssueProviderKind::Kaneo);
        k.workspace_id = "w".into();
        let mut base = IssuesConfig {
            issue_accounts: vec![k],
            ..Default::default()
        };
        // Account pinned to a workspace only: a project pin is a joint-ceiling
        // violation, a different workspace is refused.
        let refused = IssuesOverlay {
            kaneo: KaneoOverlay {
                workspace_id: Some("w2".into()),
                project_id: Some("p".into()),
            },
            ..Default::default()
        }
        .apply(&mut base);
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert_eq!(base.issue_accounts[0].workspace_id, "w");
        assert_eq!(base.issue_accounts[0].project_id, "");
    }

    #[test]
    fn overlay_pin_does_not_touch_other_provider_accounts() {
        let mut base = IssuesConfig {
            issue_accounts: vec![acct("j", IssueProviderKind::Jira)],
            ..Default::default()
        };
        let refused = IssuesOverlay {
            linear: LinearOverlay {
                team_id: Some("T".into()),
                workspace_slug: None,
            },
            ..Default::default()
        }
        .apply(&mut base);
        assert!(refused.is_empty());
        assert_eq!(base.issue_accounts[0].team_id, "");
    }

    #[test]
    fn overlay_apply_reports_each_refused_widening() {
        let mut base = IssuesConfig {
            providers: vec![IssueProviderKind::Linear],
            linear: LinearConfig {
                team_id: "G".into(),
                ..Default::default()
            },
            kaneo: KaneoConfig {
                workspace_id: "w".into(),
                project_id: "p".into(),
                ..Default::default()
            },
            jira: JiraConfig {
                project_key: "J".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let refused = IssuesOverlay {
            providers: Some(vec![
                IssueProviderKind::Linear,
                IssueProviderKind::Jira,
                IssueProviderKind::None,
            ]),
            linear: LinearOverlay {
                team_id: Some("X".into()),
                workspace_slug: None,
            },
            jira: JiraOverlay {
                project_key: Some("Y".into()),
            },
            kaneo: KaneoOverlay {
                workspace_id: Some("w2".into()),
                project_id: Some("p2".into()),
            },
            ..Default::default()
        }
        .apply(&mut base);
        assert_eq!(refused.len(), 5, "{refused:?}");
        assert_eq!(base.active_providers(), vec![IssueProviderKind::Linear]);
        assert_eq!(base.linear.team_id, "G");
        assert_eq!(base.jira.project_key, "J");
        assert_eq!(base.kaneo.workspace_id, "w");
        assert_eq!(base.kaneo.project_id, "p");
    }

    #[test]
    fn kaneo_pins_are_one_nested_ceiling() {
        let kaneo = |ws: &str, proj: &str| IssuesConfig {
            provider: IssueProviderKind::Kaneo,
            kaneo: KaneoConfig {
                workspace_id: ws.into(),
                project_id: proj.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let ov = |ws: Option<&str>, proj: Option<&str>| IssuesOverlay {
            kaneo: KaneoOverlay {
                workspace_id: ws.map(Into::into),
                project_id: proj.map(Into::into),
            },
            ..Default::default()
        };
        // Global workspace pinned, project not: a repo project is refused.
        let mut b = kaneo("w", "");
        assert_eq!(ov(None, Some("p")).apply(&mut b).len(), 1);
        assert_eq!(b.kaneo.project_id, "");
        // Global project pinned, workspace not: a repo workspace is refused.
        let mut b = kaneo("", "p");
        assert_eq!(ov(Some("w"), None).apply(&mut b).len(), 1);
        assert_eq!(b.kaneo.workspace_id, "");
        // Neither pinned: both accepted.
        let mut b = kaneo("", "");
        assert!(ov(Some("w"), Some("p")).apply(&mut b).is_empty());
        assert_eq!(
            (b.kaneo.workspace_id.as_str(), b.kaneo.project_id.as_str()),
            ("w", "p")
        );
    }

    #[test]
    fn overlay_providers_filter_accounts_in_accounts_mode() {
        let acct = |n: &str, p| IssueAccount {
            name: n.into(),
            provider: p,
            enabled: true,
            ..Default::default()
        };
        let mut base = IssuesConfig {
            issue_accounts: vec![
                acct("l", IssueProviderKind::Linear),
                acct("j", IssueProviderKind::Jira),
            ],
            ..Default::default()
        };
        let refused = IssuesOverlay {
            providers: Some(vec![IssueProviderKind::Jira, IssueProviderKind::Github]),
            ..Default::default()
        }
        .apply(&mut base);
        // Jira is enabled via an account (no diagnostic); github is not.
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("github"));
        let names: Vec<_> = base.active_accounts().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["j".to_string()]);
    }

    #[test]
    fn refusal_is_logged_once_per_root_and_message() {
        let r1 = std::path::Path::new("/tmp/the724-a");
        let r2 = std::path::Path::new("/tmp/the724-b");
        assert!(first_refusal(r1, "m"));
        assert!(!first_refusal(r1, "m"));
        assert!(first_refusal(r1, "other"));
        assert!(first_refusal(r2, "m"));
    }
}
