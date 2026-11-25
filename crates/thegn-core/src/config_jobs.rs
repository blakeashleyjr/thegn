//! `[jobs]` — which configured env runs `thegn job run`.
//!
//! The selected runner is an explicit owner choice, never a placement
//! decision: it names an `[env.<name>]` whose provider is `sprites` or
//! `vercel`. The same env's token reference, account, project binding and
//! region are used; nothing here holds a credential.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{EnvConfig, EnvProviderKind};

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct JobsConfig {
    /// The `[env.<name>]` that runs `thegn job run`. Its provider must be
    /// `sprites` or `vercel`. Empty = no runner selected (`job run` then needs
    /// `--runner <env>`).
    pub runner: String,
    /// Seconds a routed job waits for the runner to admit it before failing
    /// closed (a runner that is transiently unavailable). 0 = do not wait.
    pub admission_timeout_secs: u64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            runner: String::new(),
            admission_timeout_secs: DEFAULT_ADMISSION_TIMEOUT_SECS,
        }
    }
}

/// Default bound on the admission wait of a routed job.
pub const DEFAULT_ADMISSION_TIMEOUT_SECS: u64 = 30;

impl JobsConfig {
    /// Semantic checks: a selected runner must be a configured Sprites or Vercel
    /// env, so a typo fails at `config validate` and not at the first job.
    pub fn validate(&self, prefix: &str, envs: &BTreeMap<String, EnvConfig>) -> Vec<String> {
        let name = self.runner.trim();
        if name.is_empty() {
            return Vec::new();
        }
        let key = format!("{prefix}.runner");
        match envs.get(name) {
            None => vec![format!("{key}: no [env.{name}] is configured")],
            Some(env) => match EnvProviderKind::of(&env.provider.provider) {
                EnvProviderKind::Sprites | EnvProviderKind::Vercel => Vec::new(),
                _ => vec![format!(
                    "{key}: [env.{name}] must use provider \"sprites\" or \"vercel\""
                )],
            },
        }
    }
}

/// Trusted per-project refinement of `[jobs]` (`[workspace.<slug>.jobs]`, also
/// spelled `[project.<slug>.jobs]`). Repo-authored `.thegn.*` files have no
/// corresponding field: which provider account runs a project's builds, and at
/// whose expense, is an owner decision.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct JobsOverlay {
    /// This project's runner `[env.<name>]` (provider `sprites` or `vercel`).
    /// Wins over the global `[jobs] runner` for this project only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// Automatic routing of a stock terminal agent's `cargo build` / `cargo
    /// test` through this project's runner (`"auto"`), or not (`"off"`, the
    /// default). Opt-in per project; there is no global spelling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

/// Whether a project's qualified build/test commands are routed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RouteMode {
    /// Nothing is intercepted; `thegn job run` is the only way in.
    #[default]
    Off,
    /// The shell shim forwards qualified recipes to the project's runner.
    Auto,
}

impl RouteMode {
    /// Parse the config spelling; `None` for anything unknown.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "" | "off" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
        }
    }
}

impl JobsOverlay {
    pub fn is_empty(&self) -> bool {
        self.runner.is_none() && self.route.is_none()
    }

    /// Semantic checks, same rule as the global key for `runner`; `route` must
    /// be `"off"` or `"auto"`.
    pub fn validate(&self, prefix: &str, envs: &BTreeMap<String, EnvConfig>) -> Vec<String> {
        let mut errs = JobsConfig {
            runner: self.runner.clone().unwrap_or_default(),
            ..JobsConfig::default()
        }
        .validate(prefix, envs);
        if let Some(route) = self.route.as_deref()
            && RouteMode::parse(route).is_none()
        {
            errs.push(format!(
                "{prefix}.route: \"{route}\" is not valid; use \"off\" or \"auto\""
            ));
        }
        errs
    }
}

/// Where the effective runner for a project came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerSource {
    /// The project's trusted `[workspace.<slug>.jobs] runner`.
    Project,
    /// The global `[jobs] runner`.
    Global,
    /// Nothing selects a runner: the project stays local.
    Unset,
    /// The project's overlay block is ambiguous, so nothing applies (fail
    /// closed: the global runner is NOT used either).
    Refused(String),
}

/// The effective runner for one project and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerChoice {
    /// The selected `[env.<name>]`; empty when `source` is `Unset`/`Refused`.
    pub env: String,
    pub source: RunnerSource,
}

impl RunnerChoice {
    /// One-line reason for the route inspector.
    pub fn reason(&self) -> String {
        match &self.source {
            RunnerSource::Project => "project binding ([project.<slug>.jobs] runner)".into(),
            RunnerSource::Global => "global default ([jobs] runner)".into(),
            RunnerSource::Unset => {
                "no runner selected for this project: builds and tests stay local".into()
            }
            RunnerSource::Refused(why) => {
                format!("project overlay refused, nothing applies: {why}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EnvProviderConfig};

    fn env(provider: &str) -> EnvConfig {
        EnvConfig {
            provider: EnvProviderConfig {
                provider: provider.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn defaults_to_no_runner_and_parses_the_key() {
        assert_eq!(Config::default().jobs.runner, "");
        let cfg: Config = toml::from_str("[jobs]\nrunner = \"ci-sprite\"\n").unwrap();
        assert_eq!(cfg.jobs.runner, "ci-sprite");
    }

    #[test]
    fn the_overlay_is_validated_like_the_global_key() {
        let mut envs = BTreeMap::new();
        envs.insert("fl".to_string(), env("fly"));
        assert!(JobsOverlay::default().validate("p.jobs", &envs).is_empty());
        let bad = JobsOverlay {
            runner: Some("fl".into()),
            ..Default::default()
        };
        let errs = bad.validate("workspace.x.jobs", &envs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("workspace.x.jobs.runner"), "{errs:?}");
    }

    #[test]
    fn the_route_key_is_off_or_auto() {
        assert_eq!(RouteMode::parse("auto"), Some(RouteMode::Auto));
        assert_eq!(RouteMode::parse(" off "), Some(RouteMode::Off));
        assert_eq!(RouteMode::parse(""), Some(RouteMode::Off));
        assert_eq!(RouteMode::parse("always"), None);
        let envs = BTreeMap::new();
        let ok = JobsOverlay {
            route: Some("auto".into()),
            ..Default::default()
        };
        assert!(ok.validate("p.jobs", &envs).is_empty());
        assert!(!ok.is_empty());
        let bad = JobsOverlay {
            route: Some("sometimes".into()),
            ..Default::default()
        };
        let errs = bad.validate("project.x.jobs", &envs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("project.x.jobs.route"), "{errs:?}");
    }

    #[test]
    fn an_empty_runner_is_always_valid() {
        assert!(
            JobsConfig::default()
                .validate("jobs", &BTreeMap::new())
                .is_empty()
        );
    }

    #[test]
    fn the_runner_must_name_a_sprites_or_vercel_env() {
        let mut envs = BTreeMap::new();
        envs.insert("sp".to_string(), env("sprites"));
        envs.insert("vx".to_string(), env("vercel"));
        envs.insert("fl".to_string(), env("fly"));
        let jobs = |runner: &str| JobsConfig {
            runner: runner.into(),
            ..JobsConfig::default()
        };
        assert!(jobs("sp").validate("jobs", &envs).is_empty());
        assert!(jobs("vx").validate("jobs", &envs).is_empty());
        let wrong = jobs("fl").validate("jobs", &envs);
        assert_eq!(wrong.len(), 1);
        assert!(wrong[0].contains("jobs.runner"), "{wrong:?}");
        let missing = jobs("nope").validate("jobs", &envs);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("no [env.nope]"), "{missing:?}");
    }

    #[test]
    fn the_admission_wait_defaults_to_thirty_seconds_and_parses() {
        assert_eq!(JobsConfig::default().admission_timeout_secs, 30);
        let parsed: JobsConfig = toml::from_str("admission_timeout_secs = 5").unwrap();
        assert_eq!(parsed.admission_timeout_secs, 5);
        assert!(parsed.runner.is_empty());
        let empty: JobsConfig = toml::from_str("").unwrap();
        assert_eq!(empty.admission_timeout_secs, 30);
    }
}
