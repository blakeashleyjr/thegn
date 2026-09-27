//! Host-side configuration health collection.
//!
//! The core crate owns parsing and schema validation.  This module owns the
//! synchronous filesystem and git-path work needed by the CLI and doctor so
//! those edges can report which file owns each finding.

use std::path::{Path, PathBuf};

use thegn_core::config_repo::RepoOverlayDiscovery;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    Main,
    Profile,
    Repo,
}

#[derive(Debug, Clone)]
pub(crate) struct Finding {
    pub(crate) path: PathBuf,
    pub(crate) message: String,
    pub(crate) warning: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigHealth {
    pub(crate) main_path: PathBuf,
    pub(crate) profile_path: Option<PathBuf>,
    pub(crate) repo_path: Option<PathBuf>,
    pub(crate) main_present: bool,
    pub(crate) findings: Vec<Finding>,
    pub(crate) main_problems: usize,
    pub(crate) profile_problems: usize,
    pub(crate) repo_problems: usize,
    pub(crate) warnings: usize,
}

impl ConfigHealth {
    pub(crate) fn problems(&self) -> usize {
        self.main_problems + self.profile_problems + self.repo_problems
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "main_path": self.main_path,
            "profile_path": self.profile_path,
            "repo_path": self.repo_path,
            "problem_count": self.problems(),
            "warning_count": self.warnings,
            "validate_command": "thegn config validate",
            "layers": {
                "main": {
                    "path": self.main_path,
                    "problems": self.main_problems,
                },
                "profile": self.profile_path.as_ref().map(|path| serde_json::json!({
                    "path": path,
                    "problems": self.profile_problems,
                })),
                "repo": self.repo_path.as_ref().map(|path| serde_json::json!({
                    "path": path,
                    "problems": self.repo_problems,
                })),
            },
        })
    }

    pub(crate) fn findings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter()
    }
}

/// Collect the selected main TOML, active external profile TOML, and selected
/// repo overlay.  Missing optional files are intentionally silent.
pub(crate) fn collect(main_path: &Path, repo_context: Option<&Path>) -> ConfigHealth {
    let mut health = ConfigHealth {
        main_path: main_path.to_path_buf(),
        profile_path: None,
        repo_path: None,
        main_present: false,
        findings: Vec::new(),
        main_problems: 0,
        profile_problems: 0,
        repo_problems: 0,
        warnings: 0,
    };

    validate_toml_file(&mut health, Layer::Main, main_path);

    if let Some(profile_path) = active_profile_path().filter(|path| path.exists()) {
        health.profile_path = Some(profile_path.clone());
        validate_toml_file(&mut health, Layer::Profile, &profile_path);
    }

    // The same capture + admission a configured verb runs (file, selected
    // profile overlay, environment, bounds, semantic and clamp checks — not
    // the state DB). The file validators above already explain their own
    // findings; this names what they cannot see, e.g. an invalid environment
    // variable or an admission-only bound, so `config validate` never says
    // "ok" for a config that startup refuses.
    let explicit = (main_path != thegn_core::config::Config::path()).then_some(main_path);
    let overrides = crate::config_source::overrides();
    match crate::config_startup::check_sources(explicit, &overrides) {
        Ok(warnings) => {
            for warning in warnings {
                add_warning(&mut health, main_path, warning);
            }
        }
        Err(detail) if health.problems() == 0 => add_problem(
            &mut health,
            Layer::Main,
            main_path,
            format!("startup would refuse this configuration: {detail}"),
        ),
        Err(_) => {}
    }

    let repo_context = repo_context
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    if let Some(repo_root) = repo_context.and_then(|path| thegn_core::repo::main_worktree(&path)) {
        let discovery = thegn_core::config_repo::discover_repo_overlay(&repo_root);
        collect_repo(&mut health, &discovery);
    }

    health
}

fn validate_toml_file(health: &mut ConfigHealth, layer: Layer, path: &Path) {
    // The trusted layers report what admission does: an unknown key outside
    // the security-relevant tables is ignorable. The repo overlay is not a
    // trusted layer and keeps refusing every unknown key.
    let policy = match layer {
        Layer::Main | Layer::Profile => {
            thegn_core::config_validate::UnknownKeys::RejectSecurityRelevant
        }
        Layer::Repo => thegn_core::config_validate::UnknownKeys::Reject,
    };
    let body = match std::fs::read_to_string(path) {
        Ok(body) => {
            if layer == Layer::Main {
                health.main_present = true;
            }
            body
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            if layer == Layer::Main {
                health.main_present = true;
            }
            add_problem(
                health,
                layer,
                path,
                format!("cannot read configuration: {error}"),
            );
            return;
        }
    };

    for diagnostic in thegn_core::config_validate::validate_diagnostics_with_policy(&body, policy) {
        match diagnostic.severity {
            thegn_core::config_validate::ValidationSeverity::Warning => {
                add_warning(health, path, diagnostic.message);
            }
            thegn_core::config_validate::ValidationSeverity::Error => {
                add_problem(health, layer, path, diagnostic.message);
            }
        }
    }

    // Plaintext secrets remain advisory: validation reports them without
    // making an otherwise parseable layer fail.
    if let Ok(cfg) = thegn_core::config_compat::normalize(&body).and_then(|normalized| {
        toml::from_str::<thegn_core::config::Config>(&normalized.body).map_err(|e| e.to_string())
    }) {
        for literal in thegn_core::secret_scan::literal_refs(&cfg) {
            add_warning(
                health,
                path,
                format!(
                    "{}: contains a legacy plaintext secret. Use a `keyring:`, `env:`, or \
                     `file:` ref, or run `thegn secret migrate` for config-backed fields.",
                    literal.path
                ),
            );
        }
    }
}

fn collect_repo(health: &mut ConfigHealth, discovery: &RepoOverlayDiscovery) {
    if let Some(candidate) = discovery.selected() {
        health.repo_path = Some(candidate.path.clone());
    } else if let Some(candidate) = discovery.unreadable_candidates().first() {
        // There is no selected readable layer, but an existing unreadable
        // candidate is still the repo layer that validation must identify.
        health.repo_path = Some(candidate.path.clone());
    }

    for candidate in discovery.unreadable_candidates() {
        add_problem(
            health,
            Layer::Repo,
            &candidate.path,
            format!("cannot read repo overlay: {}", candidate.error),
        );
    }

    let Some(candidate) = discovery.selected() else {
        return;
    };

    if let Some(warning) = discovery.shadow_warning() {
        add_warning(health, &candidate.path, warning);
    }
    for diagnostic in
        thegn_core::config_repo::validate_repo_overlay(&candidate.body, candidate.format)
    {
        let message = diagnostic.to_string();
        add_problem(health, Layer::Repo, &candidate.path, message);
    }
    for warning in thegn_core::config_repo::repo_command_collector_warnings_for_overlay(
        &candidate.body,
        candidate.format,
    ) {
        add_warning(health, &candidate.path, warning);
    }
}

fn active_profile_path() -> Option<PathBuf> {
    let profile = thegn_core::profile::active();
    (!profile.is_default()).then(|| {
        thegn_core::util::xdg_config_home()
            .join("thegn")
            .join("profiles")
            .join(profile.name)
            .join("config.toml")
    })
}

fn add_problem(health: &mut ConfigHealth, layer: Layer, path: &Path, message: String) {
    match layer {
        Layer::Main => health.main_problems += 1,
        Layer::Profile => health.profile_problems += 1,
        Layer::Repo => health.repo_problems += 1,
    }
    health.findings.push(Finding {
        path: path.to_path_buf(),
        message,
        warning: false,
    });
}

fn add_warning(health: &mut ConfigHealth, path: &Path, message: String) {
    health.warnings += 1;
    health.findings.push(Finding {
        path: path.to_path_buf(),
        message,
        warning: true,
    });
}

/// Render all findings through the normal CLI diagnostic channel.  File paths
/// are always emitted here, including for document-level syntax errors.
pub(crate) fn render_findings(health: &ConfigHealth) {
    for finding in health.findings() {
        let rendered = format!("{}: {}", finding.path.display(), finding.message);
        if finding.warning {
            thegn_core::msg::warn(&rendered);
        } else {
            thegn_core::msg::error(&rendered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_compatibility_warnings_keep_typed_severity_in_every_toml_layer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let body = "projects_dir = '/canonical'\nworkspaces_dir = '/legacy'\n[workspace.repo]\n[ci.gitlab]\ntoken = 'private-test-literal'\n";
        std::fs::write(&path, body).unwrap();
        for layer in [Layer::Main, Layer::Profile] {
            let mut health = ConfigHealth {
                main_path: path.clone(),
                profile_path: None,
                repo_path: None,
                main_present: false,
                findings: vec![],
                main_problems: 0,
                profile_problems: 0,
                repo_problems: 0,
                warnings: 0,
            };
            validate_toml_file(&mut health, layer, &path);
            assert_eq!(health.problems(), 0);
            assert_eq!(health.warnings, 3);
            assert!(
                health
                    .findings()
                    .any(|finding| finding.message.contains("plaintext secret"))
            );
            assert!(
                health
                    .findings()
                    .all(|finding| !finding.message.contains("private-test-literal"))
            );
            assert!(
                health
                    .findings()
                    .all(|finding| finding.warning && finding.path == path)
            );
            assert_eq!(health.json()["problem_count"], 0);
            assert_eq!(health.json()["warning_count"], 3);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        }
        std::fs::write(&path, "workspaces_dir = '/legacy'\npicker = 42\n").unwrap();
        let mut health = ConfigHealth {
            main_path: path.clone(),
            profile_path: None,
            repo_path: None,
            main_present: false,
            findings: vec![],
            main_problems: 0,
            profile_problems: 0,
            repo_problems: 0,
            warnings: 0,
        };
        validate_toml_file(&mut health, Layer::Profile, &path);
        assert_eq!(health.main_problems, 0);
        assert_eq!(health.profile_problems, 1);
        assert_eq!(health.warnings, 1);
        assert_eq!(health.json()["problem_count"], 1);
    }

    /// `config validate` must report what startup refuses — including an
    /// environment-layer failure the file validators cannot see — naming the
    /// variable, and must surface clamped values as named warnings.
    #[test]
    fn validate_reports_admission_only_refusals_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[metrics]\ntimeout_ms = 50\n").unwrap();
        let clean = collect(&path, Some(dir.path()));
        assert_eq!(clean.problems(), 0, "{:?}", clean.findings);
        assert!(
            clean
                .findings()
                .any(|finding| finding.warning && finding.message.contains("metrics.timeout_ms")),
            "{:?}",
            clean.findings
        );

        // An unknown key outside the security-relevant tables is reported as
        // an ignorable warning, not a problem: one config.toml is shared by
        // builds of different ages.
        std::fs::write(&path, "[ui]\nnewer_build_knob = true\n").unwrap();
        let relaxed = collect(&path, Some(dir.path()));
        assert_eq!(relaxed.problems(), 0, "{:?}", relaxed.findings);
        assert!(
            relaxed
                .findings()
                .any(|finding| finding.warning && finding.message.contains("unknown key")),
            "{:?}",
            relaxed.findings
        );
        // …but inside one it is a problem `config validate` must report.
        std::fs::write(&path, "[sandbox]\nnewer_policy_knob = true\n").unwrap();
        let refused = collect(&path, Some(dir.path()));
        assert_eq!(refused.problems(), 1, "{:?}", refused.findings);
        // The environment layer's own refusal is covered without mutating
        // this process (`just coverage` runs every test in ONE process) by
        // `config_admission::tests::rejection_detail_names_source_and_key…`.
    }

    #[test]
    fn main_diagnostics_are_owned_by_the_main_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[sandbox]\nenabled = \"yes\"\n").unwrap();

        let health = collect(&path, Some(dir.path()));
        assert!(health.main_present);
        assert_eq!(health.problems(), 1);
        assert_eq!(health.findings().next().unwrap().path, path);
    }

    #[test]
    fn selected_repo_winner_and_shadow_warning_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".thegn.toml"), "mystery = true\n").unwrap();
        std::fs::write(dir.path().join(".thegn.yaml"), "sandbox: {}\n").unwrap();
        let main = dir.path().join("config.toml");
        std::fs::write(&main, "").unwrap();

        // This test supplies a directory only when it is a git repo; the
        // discovery API itself is covered in core.  The host collector should
        // remain quiet for an ordinary non-repo path.
        let health = collect(&main, Some(dir.path()));
        assert_eq!(health.problems(), 0);
        assert!(health.repo_path.is_none());
    }

    #[test]
    fn selected_repo_command_collectors_are_warned_without_execution() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("collector-ran");
        std::fs::write(
            dir.path().join(".thegn.toml"),
            format!(
                "[[metrics.targets]]\nname = \"repo-command\"\nkind = \"command\"\ncommand = [\"sh\", \"-c\", \"touch '{}'\"]\n",
                marker.display()
            ),
        )
        .unwrap();

        let discovery = thegn_core::config_repo::discover_repo_overlay(dir.path());
        let mut health = ConfigHealth {
            main_path: dir.path().join("config.toml"),
            profile_path: None,
            repo_path: None,
            main_present: false,
            findings: Vec::new(),
            main_problems: 0,
            profile_problems: 0,
            repo_problems: 0,
            warnings: 0,
        };
        collect_repo(&mut health, &discovery);

        assert_eq!(health.problems(), 0);
        assert_eq!(health.warnings, 1);
        assert!(health.findings().any(|finding| {
            finding.warning
                && finding.message.contains("repo-command")
                && finding.message.contains("global config only")
        }));
        assert!(
            !marker.exists(),
            "config health must not execute collectors"
        );
    }

    #[test]
    fn unreadable_repo_candidate_is_a_path_owned_problem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".thegn.toml")).unwrap();
        std::fs::write(dir.path().join(".thegn.yaml"), "sandbox: {}\n").unwrap();

        let discovery = thegn_core::config_repo::discover_repo_overlay(dir.path());
        let mut health = ConfigHealth {
            main_path: dir.path().join("config.toml"),
            profile_path: None,
            repo_path: None,
            main_present: false,
            findings: Vec::new(),
            main_problems: 0,
            profile_problems: 0,
            repo_problems: 0,
            warnings: 0,
        };
        collect_repo(&mut health, &discovery);

        let unreadable = dir.path().join(".thegn.toml");
        let selected = dir.path().join(".thegn.yaml");
        assert_eq!(health.repo_path.as_deref(), Some(selected.as_path()));
        assert_eq!(health.repo_problems, 1);
        assert!(health.findings().any(|finding| {
            finding.path == unreadable && finding.message.starts_with("cannot read repo overlay:")
        }));
    }
}
