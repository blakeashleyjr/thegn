//! Plugin discovery + validation: `[[plugins]]` config entries merged with
//! `<config_dir>/plugins/<dir>/plugin.toml` directories, each checked against
//! the host contract before anything runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::plugin_api::{
    API_VERSION, ExtensionPoint, HostContract, NegotiatedManifest, PluginSpec,
};

/// The extension points this host build actually renders/consumes. Grows as
/// surfaces land (PaletteAction/SidebarTab are next; providers are the
/// provider-as-plugin phase).
pub fn host_contract() -> HostContract {
    HostContract::new(API_VERSION)
        .with_extension_points([
            ExtensionPoint::StatusBarSegment,
            ExtensionPoint::NotificationSource,
        ])
        // The surface capabilities those extension points require: declaring
        // the point implies granting its surface (registration would
        // otherwise always be denied).
        .with_grants([
            thegn_core::plugin_api::Capability::new("surface", "statusbar"),
            thegn_core::plugin_api::Capability::new("surface", "notification"),
        ])
}

/// One discovered plugin: where it came from and what to run.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub spec: PluginSpec,
    /// `None` for `[[plugins]]` config entries; the plugin directory for
    /// `plugins/<dir>/plugin.toml` discoveries.
    pub dir: Option<PathBuf>,
}

impl LoadedPlugin {
    /// The working directory the plugin's process should run in: an explicit
    /// `cwd` wins, then the plugin's own directory, then the host cwd.
    pub fn effective_cwd(&self) -> Option<PathBuf> {
        let cwd = self.spec.cwd.trim();
        if !cwd.is_empty() {
            return Some(PathBuf::from(cwd));
        }
        self.dir.clone()
    }
}

/// A validation problem `thegn plugin check` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecProblem {
    pub plugin: String,
    pub problem: String,
}

impl std::fmt::Display for SpecProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.plugin, self.problem)
    }
}

/// Discover every plugin: config `[[plugins]]` first (order preserved), then
/// `<config_dir>/plugins/*/plugin.toml` sorted by directory name. Disabled
/// specs are kept (the CLI lists them); duplicates by id keep the first and
/// report the clash via [`check_specs`].
pub fn discover(cfg: &Config, config_dir: &Path) -> Vec<LoadedPlugin> {
    let mut out: Vec<LoadedPlugin> = cfg
        .plugins
        .iter()
        .cloned()
        .map(|spec| LoadedPlugin { spec, dir: None })
        .collect();
    let plugins_dir = config_dir.join("plugins");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&plugins_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("plugin.toml").is_file())
        .collect();
    dirs.sort();
    for dir in dirs {
        let manifest_path = dir.join("plugin.toml");
        match std::fs::read_to_string(&manifest_path)
            .map_err(|e| e.to_string())
            .and_then(|s| toml::from_str::<PluginSpec>(&s).map_err(|e| e.to_string()))
        {
            Ok(spec) => out.push(LoadedPlugin {
                spec,
                dir: Some(dir),
            }),
            Err(e) => {
                // A broken manifest must be visible in `plugin check`, so it
                // is surfaced there (check_specs re-parses); at discovery it
                // is logged and skipped rather than taking the set down.
                tracing::warn!(target: "thegn::plugin", path = %manifest_path.display(), error = %e, "unparseable plugin.toml");
            }
        }
    }
    out
}

/// Manifest files that exist but do not parse (for `plugin check`).
pub fn broken_manifests(config_dir: &Path) -> Vec<SpecProblem> {
    let plugins_dir = config_dir.join("plugins");
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&plugins_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("plugin.toml").is_file())
        .collect();
    dirs.sort();
    for dir in dirs {
        let path = dir.join("plugin.toml");
        if let Err(e) = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| toml::from_str::<PluginSpec>(&s).map_err(|e| e.to_string()))
        {
            out.push(SpecProblem {
                plugin: dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                problem: format!("plugin.toml does not parse: {e}"),
            });
        }
    }
    out
}

/// Negotiate one spec against the host contract.
pub fn negotiate(spec: &PluginSpec) -> Result<NegotiatedManifest, String> {
    host_contract()
        .negotiate(&spec.manifest)
        .map_err(|e| e.to_string())
}

/// Validate one loaded plugin; empty = clean.
pub fn check_spec(p: &LoadedPlugin) -> Vec<SpecProblem> {
    let id = p.spec.manifest.id.as_str().to_string();
    let mut out = Vec::new();
    let mut push = |problem: String| {
        out.push(SpecProblem {
            plugin: id.clone(),
            problem,
        })
    };
    if id.trim().is_empty() {
        push("empty plugin id".into());
    }
    match p.spec.command.first() {
        None => push("empty command".into()),
        Some(program) => {
            // A bare program must resolve on PATH; a path (absolute or
            // plugin-dir-relative) must exist and the relative form is
            // resolved against the effective cwd.
            let is_pathy = program.contains(std::path::MAIN_SEPARATOR) || program.contains('/');
            if is_pathy {
                let candidate = PathBuf::from(program);
                let resolved = if candidate.is_absolute() {
                    candidate
                } else {
                    p.effective_cwd().unwrap_or_default().join(candidate)
                };
                if !resolved.is_file() {
                    push(format!("command not found: {}", resolved.display()));
                }
            } else if thegn_core::util::which_path(program).is_none() {
                push(format!("command not on PATH: {program}"));
            }
        }
    }
    match negotiate(&p.spec) {
        Err(e) => push(e),
        Ok(neg) => {
            for c in &neg.unsupported_contributions {
                push(format!(
                    "contribution {:?} targets unsupported extension point {}",
                    c.id.as_str(),
                    format_args!("{:?}", c.extension_point)
                ));
            }
        }
    }
    out
}

/// Validate the whole discovered set (plus unparseable manifests): id
/// clashes, then per-spec problems for **enabled** plugins — a disabled
/// plugin's problems are informational, not check failures.
pub fn check_specs(cfg: &Config, config_dir: &Path) -> Vec<SpecProblem> {
    let loaded = discover(cfg, config_dir);
    let mut out = broken_manifests(config_dir);
    let mut seen: HashSet<String> = HashSet::new();
    for p in &loaded {
        let id = p.spec.manifest.id.as_str().to_string();
        if !seen.insert(id.clone()) {
            out.push(SpecProblem {
                plugin: id,
                problem: "duplicate plugin id".into(),
            });
        }
    }
    for p in loaded.iter().filter(|p| p.spec.enabled) {
        out.extend(check_spec(p));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::plugin_api::{ApiVersion, Contribution, PluginManifest};

    fn spec(id: &str, command: Vec<String>) -> PluginSpec {
        PluginSpec {
            manifest: PluginManifest {
                id: thegn_core::plugin_api::PluginId::new(id),
                name: id.to_string(),
                version: "0.1.0".into(),
                api: API_VERSION,
                capabilities: Vec::new(),
                contributions: Vec::new(),
            },
            command,
            cwd: String::new(),
            env: Default::default(),
            timeout_secs: 5,
            scopes: Vec::new(),
            mode: Default::default(),
            enabled: true,
        }
    }

    #[test]
    fn discovers_config_then_directory_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins/hello");
        std::fs::create_dir_all(&dir).unwrap();
        let s = spec("dir-hello", vec!["true".into()]);
        std::fs::write(dir.join("plugin.toml"), toml::to_string(&s).unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.plugins.push(spec("cfg-first", vec!["true".into()]));
        let loaded = discover(&cfg, tmp.path());
        let ids: Vec<_> = loaded
            .iter()
            .map(|p| p.spec.manifest.id.as_str().to_string())
            .collect();
        assert_eq!(ids, ["cfg-first", "dir-hello"]);
        // Directory plugins default their cwd to their own directory.
        assert_eq!(loaded[1].effective_cwd().as_deref(), Some(dir.as_path()));
        assert_eq!(loaded[0].effective_cwd(), None);
    }

    #[test]
    fn check_flags_missing_command_bad_api_and_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.plugins
            .push(spec("gone", vec!["thegn-no-such-plugin-binary".into()]));
        let mut bad = spec("future", vec!["true".into()]);
        bad.manifest.api = ApiVersion::new(9, 0, 0);
        cfg.plugins.push(bad);
        cfg.plugins.push(spec("gone", vec!["true".into()]));
        let problems = check_specs(&cfg, tmp.path());
        let text = problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("gone: command not on PATH"), "{text}");
        assert!(text.contains("future: incompatible api"), "{text}");
        assert!(text.contains("gone: duplicate plugin id"), "{text}");
    }

    #[test]
    fn disabled_specs_are_listed_but_not_check_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let mut s = spec("off", vec!["thegn-no-such-plugin-binary".into()]);
        s.enabled = false;
        cfg.plugins.push(s);
        assert_eq!(discover(&cfg, tmp.path()).len(), 1);
        assert!(check_specs(&cfg, tmp.path()).is_empty());
    }

    #[test]
    fn unsupported_extension_point_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = spec("theme", vec!["true".into()]);
        s.manifest.contributions.push(Contribution {
            id: thegn_core::plugin_api::ContributionId::new("t"),
            extension_point: ExtensionPoint::Theme,
            label: "T".into(),
            surface: None,
            cadence: thegn_core::plugin_api::CadenceHint::OnDemand,
            metadata: Default::default(),
            caps: serde_json::Value::Null,
            chord: None,
        });
        let mut cfg = Config::default();
        cfg.plugins.push(s);
        let problems = check_specs(&cfg, tmp.path());
        assert!(
            problems
                .iter()
                .any(|p| p.problem.contains("unsupported extension point")),
            "{problems:?}"
        );
    }

    #[test]
    fn broken_manifest_is_a_check_failure_but_not_a_discovery_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins/broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), "not = [valid").unwrap();
        let cfg = Config::default();
        assert!(discover(&cfg, tmp.path()).is_empty());
        let problems = check_specs(&cfg, tmp.path());
        assert!(
            problems
                .iter()
                .any(|p| p.problem.contains("does not parse")),
            "{problems:?}"
        );
    }
}
