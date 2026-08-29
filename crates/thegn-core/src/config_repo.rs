//! Repo-local configuration overlays.
//!
//! Repo overlays are intentionally the one untrusted configuration surface
//! with tri-format support.  This module keeps their candidate discovery and
//! parsing separate from the trusted TOML configuration and exposes the same
//! schema-driven validation substrate used by [`crate::config_validate`].

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

use crate::config::{
    KeybindConfig, MetricsTarget, MetricsTargetKind, NotificationsOverlay, SandboxOverlay,
};

/// The formats accepted for repo-local overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayFormat {
    Toml,
    Yaml,
    Json,
}

impl OverlayFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Toml => "TOML",
            Self::Yaml => "YAML",
            Self::Json => "JSON",
        }
    }

    fn from_extension(extension: &str) -> Option<Self> {
        match extension {
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

impl std::fmt::Display for OverlayFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A readable repo overlay candidate, in precedence order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverlayCandidate {
    pub path: PathBuf,
    pub format: OverlayFormat,
    pub body: String,
}

/// An existing repo overlay candidate whose contents could not be read.
///
/// The error is kept so validation can explain why the candidate was omitted;
/// its contents are never retained or included in diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverlayUnreadableCandidate {
    pub path: PathBuf,
    pub error: String,
}

/// All readable candidates, unreadable candidates, and the selected winner.
/// TOML wins, followed by YAML/YML, then JSON among readable candidates;
/// unreadable files are never selected, but remain visible to validation and
/// health reporting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoOverlayDiscovery {
    pub candidates: Vec<RepoOverlayCandidate>,
    pub unreadable: Vec<RepoOverlayUnreadableCandidate>,
}

impl RepoOverlayDiscovery {
    pub fn selected(&self) -> Option<&RepoOverlayCandidate> {
        self.candidates.first()
    }

    pub fn shadowed(&self) -> &[RepoOverlayCandidate] {
        self.candidates.get(1..).unwrap_or_default()
    }

    pub fn unreadable_candidates(&self) -> &[RepoOverlayUnreadableCandidate] {
        &self.unreadable
    }

    /// The path-only warning for a multi-candidate repo overlay.
    pub fn shadow_warning(&self) -> Option<String> {
        let winner = self.selected()?;
        if self.shadowed().is_empty() {
            return None;
        }
        let ignored = self
            .shadowed()
            .iter()
            .map(|candidate| candidate.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "using repo overlay {}; ignoring shadowed candidate(s): {ignored}",
            winner.path.display()
        ))
    }
}

/// A diagnostic produced by validating a repo overlay body.  `path` is a
/// dotted config path for value errors and is empty for document-level errors;
/// the host can prefix it with the owning file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverlayDiagnostic {
    pub format: OverlayFormat,
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for RepoOverlayDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{} {}", self.format, self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// The shape of a repo-root `.thegn.*` file.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct RepoConfigFile {
    pub(crate) sandbox: SandboxOverlay,
    pub(crate) keybinds: KeybindConfig,
    /// Per-repo notification routing overlay.
    pub(crate) notifications: NotificationsOverlay,
    /// Per-repo issue-tracker overlay.
    pub(crate) issues: crate::config_issues::IssuesOverlay,
    /// Selects a named environment for every worktree of this repo.
    pub(crate) env: String,
    /// Metrics are present only so command collectors can be detected and
    /// refused; their targets never reach the running scraper.
    pub(crate) metrics: RepoMetricsOverlay,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct RepoMetricsOverlay {
    pub(crate) targets: Vec<MetricsTarget>,
}

/// Warnings for command collectors declared in an untrusted metrics overlay.
pub(crate) fn reject_overlay_command_collectors(targets: &[MetricsTarget]) -> Vec<String> {
    targets
        .iter()
        .filter(|t| t.kind == MetricsTargetKind::Command)
        .map(|t| {
            format!(
                "ignoring metrics target '{}': command collectors are global config only \
                 (a repo .thegn.* overlay cannot run commands)",
                t.name
            )
        })
        .collect()
}

/// Discover every readable candidate without parsing it. The first candidate
/// is the winner; all later readable candidates are shadowed and remain visible
/// to validation/health callers. Existing candidates that cannot be read are
/// retained separately so a lower-precedence readable file cannot hide them.
pub fn discover_repo_overlay(repo_root: &Path) -> RepoOverlayDiscovery {
    let mut candidates = Vec::new();
    let mut unreadable = Vec::new();
    for extension in ["toml", "yaml", "yml", "json"] {
        let path = repo_root.join(format!(".thegn.{extension}"));
        let Some(format) = OverlayFormat::from_extension(extension) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(body) => candidates.push(RepoOverlayCandidate { path, format, body }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => unreadable.push(RepoOverlayUnreadableCandidate {
                path,
                error: error.to_string(),
            }),
        }
    }
    RepoOverlayDiscovery {
        candidates,
        unreadable,
    }
}

/// Parse a repo overlay into the format-neutral JSON value used by the schema
/// walker.  This function is pure with respect to the supplied body.
pub fn parse_overlay_value(body: &str, format: OverlayFormat) -> Result<serde_json::Value, String> {
    match format {
        OverlayFormat::Toml => {
            let value: toml::Value = body.parse::<toml::Value>().map_err(|e| e.to_string())?;
            serde_json::to_value(value).map_err(|e| e.to_string())
        }
        OverlayFormat::Yaml => {
            serde_yaml::from_str::<serde_json::Value>(body).map_err(|e| e.to_string())
        }
        OverlayFormat::Json => serde_json::from_str(body).map_err(|e| e.to_string()),
    }
}

pub(crate) fn parse_repo_config(
    body: &str,
    format: OverlayFormat,
) -> Result<RepoConfigFile, String> {
    match format {
        OverlayFormat::Toml => toml::from_str(body).map_err(|e| e.to_string()),
        OverlayFormat::Yaml => serde_yaml::from_str(body).map_err(|e| e.to_string()),
        OverlayFormat::Json => serde_json::from_str(body).map_err(|e| e.to_string()),
    }
}

/// Return the command-collector refusals for an already-discovered candidate.
///
/// The host health collector already owns the candidate body and format, so it
/// can apply the same trust rule without starting a second discovery/read
/// path. Syntax and schema problems are reported by [`validate_repo_overlay`];
/// an unparseable body therefore produces no additional refusal warning here.
pub fn repo_command_collector_warnings_for_overlay(
    body: &str,
    format: OverlayFormat,
) -> Vec<String> {
    parse_repo_config(body, format)
        .map(|overlay| reject_overlay_command_collectors(&overlay.metrics.targets))
        .unwrap_or_default()
}

/// Validate a repo overlay against the actual `RepoConfigFile` schema.
///
/// Loading remains tolerant: callers that only need effective values may keep
/// using the tolerant repo-overlay loader, while validation callers get all syntax,
/// unknown-key, enum, and type diagnostics without format-specific branches.
pub fn validate_repo_overlay(body: &str, format: OverlayFormat) -> Vec<RepoOverlayDiagnostic> {
    let value = match parse_overlay_value(body, format) {
        Ok(value) => value,
        Err(error) => {
            return vec![RepoOverlayDiagnostic {
                format,
                path: String::new(),
                message: format!("syntax error: {error}"),
            }];
        }
    };

    crate::config_validate::validate_schema_value::<RepoConfigFile>(&value)
        .into_iter()
        .map(|message| {
            let (path, message) = message
                .split_once(": ")
                .map_or((String::new(), message.clone()), |(path, message)| {
                    (path.to_string(), message.to_string())
                });
            RepoOverlayDiagnostic {
                format,
                path,
                message,
            }
        })
        .collect()
}

/// Load the selected repo overlay, preserving the tolerant behavior used by
/// the compositor.  A malformed winner is ignored and does not fall through
/// to a lower-precedence candidate.
pub(crate) fn load_repo_overlay(repo_root: &Path) -> Option<RepoConfigFile> {
    let discovery = discover_repo_overlay(repo_root);
    warn_about_shadowed(&discovery);
    let candidate = discovery.selected()?;
    match parse_repo_config(&candidate.body, candidate.format) {
        Ok(config) => Some(config),
        Err(error) => {
            crate::config::config_warn(&format!(
                "{}: parse error: {error}; ignoring",
                candidate.path.display()
            ));
            None
        }
    }
}

fn warn_about_shadowed(discovery: &RepoOverlayDiscovery) {
    let Some(warning) = discovery.shadow_warning() else {
        return;
    };
    static WARNED: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()));
    let Ok(mut warned) = warned.lock() else {
        return;
    };
    let warning_key = warning.clone();
    if !warned.insert(warning_key) {
        return;
    }
    crate::config::config_warn(&warning);
}

/// A repo-root overlay that exists but failed to parse.
#[derive(Debug, Clone)]
pub struct RepoOverlayParseError {
    pub path: PathBuf,
    pub error: String,
    pub selected_env: String,
}

/// Return a parse error for the selected candidate, if any.  This mirrors the
/// tolerant loader's precedence: a malformed winner does not fall through.
pub fn repo_overlay_parse_error(repo_root: &Path) -> Option<RepoOverlayParseError> {
    let discovery = discover_repo_overlay(repo_root);
    let candidate = discovery.selected()?;
    parse_repo_config(&candidate.body, candidate.format)
        .err()
        .map(|error| RepoOverlayParseError {
            path: candidate.path.clone(),
            error,
            selected_env: lenient_env_selector(&candidate.body),
        })
}

/// Best-effort extraction of a top-level `env = "VALUE"` or `env: VALUE`
/// selector from malformed overlay text.
pub(crate) fn lenient_env_selector(text: &str) -> String {
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("env") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')) else {
            continue;
        };
        let value = rest.trim().trim_matches('"').trim_matches('\'').trim();
        if !value.is_empty() && !value.starts_with('{') && !value.starts_with('[') {
            return value.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &[(OverlayFormat, &str)] = &[
        (OverlayFormat::Toml, "[sandbox]\nenabled = true\n"),
        (OverlayFormat::Yaml, "sandbox:\n  enabled: true\n"),
        (OverlayFormat::Json, r#"{"sandbox":{"enabled":true}}"#),
    ];

    #[test]
    fn valid_repo_documents_are_accepted_in_all_formats() {
        for (format, body) in VALID {
            assert!(
                validate_repo_overlay(body, *format).is_empty(),
                "{format}: {body:?}"
            );
        }
    }

    #[test]
    fn syntax_errors_name_the_source_format() {
        for (format, body) in [
            (OverlayFormat::Toml, "[sandbox\n"),
            (OverlayFormat::Yaml, "sandbox: [\n"),
            (OverlayFormat::Json, "{\"sandbox\":}"),
        ] {
            let errors = validate_repo_overlay(body, format);
            assert_eq!(errors.len(), 1, "{format}: {errors:?}");
            assert_eq!(errors[0].format, format);
            assert!(errors[0].path.is_empty());
            assert!(errors[0].message.starts_with("syntax error:"));
        }
    }

    #[test]
    fn unknown_top_level_keys_are_reported() {
        for (format, body) in [
            (OverlayFormat::Toml, "mystery = true\n"),
            (OverlayFormat::Yaml, "mystery: true\n"),
            (OverlayFormat::Json, r#"{"mystery":true}"#),
        ] {
            let errors = validate_repo_overlay(body, format);
            assert!(
                errors
                    .iter()
                    .any(|e| e.path == "mystery" && e.message == "unknown key"),
                "{format}: {errors:?}"
            );
        }
    }

    #[test]
    fn nested_typos_include_a_nearest_key_hint() {
        for (format, body) in [
            (OverlayFormat::Toml, "[sandbox]\nenabld = true\n"),
            (OverlayFormat::Yaml, "sandbox:\n  enabld: true\n"),
            (OverlayFormat::Json, r#"{"sandbox":{"enabld":true}}"#),
        ] {
            let errors = validate_repo_overlay(body, format);
            assert!(
                errors
                    .iter()
                    .any(|e| { e.path == "sandbox.enabld" && e.message.contains("enabled") }),
                "{format}: {errors:?}"
            );
        }
    }

    #[test]
    fn type_errors_name_path_expected_and_actual_types() {
        for (format, body) in [
            (OverlayFormat::Toml, "[sandbox]\nenabled = \"yes\"\n"),
            (OverlayFormat::Yaml, "sandbox:\n  enabled: yes\n"),
            (OverlayFormat::Json, r#"{"sandbox":{"enabled":"yes"}}"#),
        ] {
            let errors = validate_repo_overlay(body, format);
            assert!(
                errors.iter().any(|e| {
                    e.path == "sandbox.enabled" && e.message == "expected boolean, got string"
                }),
                "{format}: {errors:?}"
            );
        }
    }

    #[test]
    fn discovery_reports_shadowed_candidates_in_precedence_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".thegn.toml"),
            "[sandbox]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".thegn.yaml"),
            "sandbox:\n  enabled: true\n",
        )
        .unwrap();
        let discovery = discover_repo_overlay(dir.path());
        assert_eq!(discovery.selected().unwrap().format, OverlayFormat::Toml);
        assert_eq!(discovery.shadowed().len(), 1);
        assert_eq!(discovery.shadowed()[0].path, dir.path().join(".thegn.yaml"));
        let warning = discovery.shadow_warning().unwrap();
        assert!(warning.contains(&dir.path().join(".thegn.toml").display().to_string()));
        assert!(warning.contains(&dir.path().join(".thegn.yaml").display().to_string()));
        assert!(!warning.contains("enabled = true"));
    }

    #[test]
    fn command_collector_refusal_uses_an_already_read_overlay_body() {
        let warnings = repo_command_collector_warnings_for_overlay(
            r#"
                [[metrics.targets]]
                name = "repo-command"
                kind = "command"
                command = ["should-not-run"]
            "#,
            OverlayFormat::Toml,
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("repo-command"));
    }

    #[test]
    fn discovery_retains_unreadable_candidates_and_selects_lower_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the candidate path reliably exercises a read failure
        // even when tests run with elevated permissions.
        std::fs::create_dir(dir.path().join(".thegn.toml")).unwrap();
        std::fs::write(dir.path().join(".thegn.yaml"), "sandbox: {}\n").unwrap();

        let discovery = discover_repo_overlay(dir.path());
        assert_eq!(
            discovery.selected().unwrap().path,
            dir.path().join(".thegn.yaml")
        );
        assert_eq!(discovery.unreadable_candidates().len(), 1);
        assert_eq!(
            discovery.unreadable_candidates()[0].path,
            dir.path().join(".thegn.toml")
        );
        assert!(!discovery.unreadable_candidates()[0].error.is_empty());
    }
}
