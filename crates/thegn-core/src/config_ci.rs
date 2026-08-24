//! The `[ci]` config family — cross-provider CI/CD inspection (AV group).
//! Provider-agnostic knobs plus per-provider endpoint/token sub-tables for
//! implemented providers only (reserved kinds carry no sub-table). Kept in
//! a sibling module (rather than the god-file `config.rs`) per the file-size
//! ratchet; `config.rs` re-exports everything here.

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};

config_enum! {
    /// Active CI provider for the CI panel/view (AV group). `"auto"` picks the
    /// provider from the worktree's CI-config files + git remote; `"none"`
    /// disables. GitHub reuses the existing `gh`/`GH_TOKEN` auth (no sub-table).
    pub enum CiProviderKind : "ci provider" {
        Auto       = "auto",
        None       = "none",
        Github     = "github",
        Gitlab     = "gitlab",
        // Reserved: accepted by config so a future build can implement them
        // without a config-format change, rejected by `config validate
        // --strict` today. Do not add `[ci.<kind>]` sub-tables for these —
        // the provider-seams spec forbids config surface with nothing behind
        // it. File-based `auto` detection still recognises their CI files.
        Drone      = "drone" reserved,
        Woodpecker = "woodpecker" reserved,
        Jenkins    = "jenkins" reserved,
        Argo       = "argo" reserved,
    } default = Auto;
}

/// `[ci]` — cross-provider CI/CD inspection (AV group). Provider-agnostic knobs
/// here; per-provider endpoints/tokens in the sub-tables. Tokens accept the
/// `"env:VAR"` form resolved by `expand_env_ref`, so secrets stay out of the
/// file.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CiConfig {
    /// Active provider; `"auto"` detects from the worktree.
    pub provider: CiProviderKind,
    /// Freshness window (seconds): non-forced refreshes (ticker, tab switch)
    /// skip while the cache is younger. `0` disables; `g` always refetches.
    pub ttl_secs: u64,
    /// Run-history refresh cadence (seconds), min 5 (a subprocess per poll).
    pub poll_interval_secs: u64,
    /// How many recent runs to fetch and display.
    pub max_runs: usize,
    /// Cap on fetched log lines (the tail is kept) — bounds memory on huge jobs.
    pub log_tail_lines: usize,
    pub gitlab: GitLabCiConfig,
}

impl Default for CiConfig {
    fn default() -> Self {
        CiConfig {
            provider: CiProviderKind::Auto,
            ttl_secs: 30,
            poll_interval_secs: 30,
            max_runs: 50,
            log_tail_lines: 2000,
            gitlab: GitLabCiConfig::default(),
        }
    }
}

/// `[ci.gitlab]` — GitLab CI. `host` empty ⇒ gitlab.com; set it for self-hosted.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct GitLabCiConfig {
    pub host: String,
    /// API token. Use `"env:GITLAB_TOKEN"` to read from the environment.
    pub token: String,
}

impl Default for GitLabCiConfig {
    fn default() -> Self {
        GitLabCiConfig {
            host: String::new(),
            token: "env:GITLAB_TOKEN".into(),
        }
    }
}
