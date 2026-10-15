//! Plugin discovery + validation: `[[plugins]]` config entries merged with
//! `<config_dir>/plugins/<dir>/plugin.toml` directories, each checked against
//! the host contract before anything runs.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::plugin_api::{
    API_VERSION, CadenceHint, Capability, HOST_EXTENSION_SUPPORT, HostContract, NegotiatedManifest,
    PluginSpec, SupportState,
};

pub use thegn_core::plugin_api::MAX_PLUGIN_TIMEOUT_SECS;

pub const MIN_PLUGIN_ID_BYTES: usize = 1;
pub const MAX_PLUGIN_ID_BYTES: usize = 64;
pub const MIN_PLUGIN_TIMEOUT_SECS: u64 = 1;
pub const MIN_PLUGIN_INTERVAL_MILLIS: u64 = 100;
pub const MAX_PLUGIN_INTERVAL_MILLIS: u64 = 86_400_000;
/// Maximum discovered plugin directories that contain a `plugin.toml`.
pub const MAX_PLUGIN_DIRS: usize = 256;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MIN_PLUGIN_ROUTING_ID_BYTES: usize = 1;
pub const MAX_PLUGIN_ROUTING_ID_BYTES: usize = 128;

/// The extension points this host build actually renders/consumes. The wire
/// enum is intentionally broader; a point belongs here only after its complete
/// host runtime, placement, and activation path has landed.
pub fn host_contract() -> HostContract {
    HostContract::new(API_VERSION)
        .with_extension_support(HOST_EXTENSION_SUPPORT.iter().cloned())
        // Derive surface grants from the same rows that decide negotiation.
        // A newly wired point therefore cannot be accepted while its required
        // capability is accidentally left in a second hand-maintained list.
        .with_grants(
            HOST_EXTENSION_SUPPORT
                .iter()
                .filter(|row| row.state == SupportState::Wired)
                .filter_map(|row| row.required_capability)
                .filter_map(Capability::parse),
        )
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
/// specs are kept (the CLI lists them), and duplicate ids remain in the
/// result so [`check_loaded`] can reject the complete set.
///
/// Parse failures are returned from the same directory pass as the loaded
/// plugins, so callers can validate the exact snapshot they discovered.
pub fn discover(cfg: &Config, config_dir: &Path) -> (Vec<LoadedPlugin>, Vec<SpecProblem>) {
    let mut out: Vec<LoadedPlugin> = cfg
        .plugins
        .iter()
        .cloned()
        .map(|spec| LoadedPlugin { spec, dir: None })
        .collect();
    let plugins_dir = config_dir.join("plugins");
    let mut problems = Vec::new();
    match std::fs::metadata(&plugins_dir) {
        Ok(metadata) if !metadata.is_dir() => {
            problems.push(discovery_problem(
                &plugins_dir,
                "plugins path is not a directory".into(),
            ));
            return (out, problems);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (out, problems),
        Err(error) => {
            problems.push(discovery_problem(&plugins_dir, error.to_string()));
            return (out, problems);
        }
    }

    let entries = match std::fs::read_dir(&plugins_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (out, problems),
        Err(error) => {
            problems.push(discovery_problem(&plugins_dir, error.to_string()));
            return (out, problems);
        }
    };
    let mut dirs = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                match entry.file_type() {
                    Ok(file_type) if file_type.is_symlink() => problems.push(discovery_problem(
                        &path,
                        "plugin entry must not be a symlink".into(),
                    )),
                    Ok(file_type) if file_type.is_dir() => {
                        dirs.push(path);
                    }
                    Ok(_) => {} // Files such as README are not plugin directories.
                    Err(error) => problems.push(discovery_problem(&path, error.to_string())),
                }
            }
            Err(error) => problems.push(discovery_problem(&plugins_dir, error.to_string())),
        }
    }
    dirs.sort();
    let mut manifests = Vec::new();
    for dir in dirs {
        let manifest_path = dir.join("plugin.toml");
        let metadata = match std::fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                problems.push(discovery_problem(&dir, error.to_string()));
                continue;
            }
        };
        manifests.push((dir, metadata));
    }
    if manifests.len() > MAX_PLUGIN_DIRS {
        problems.push(discovery_problem(
            &plugins_dir,
            format!("plugin directory count exceeds {MAX_PLUGIN_DIRS}"),
        ));
        problems.sort_by(|left, right| {
            (&left.plugin, &left.problem).cmp(&(&right.plugin, &right.problem))
        });
        return (out, problems);
    }
    for (dir, metadata) in manifests {
        let manifest_path = dir.join("plugin.toml");
        match metadata {
            metadata if metadata.file_type().is_symlink() => {
                problems.push(discovery_problem(
                    &dir,
                    "plugin.toml must not be a symlink".into(),
                ));
                continue;
            }
            metadata if !metadata.is_file() => {
                problems.push(discovery_problem(
                    &dir,
                    "plugin.toml must be a regular file".into(),
                ));
                continue;
            }
            _ => {}
        }
        let parsed = std::fs::File::open(&manifest_path)
            .map_err(|error| error.to_string())
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(MAX_MANIFEST_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                if bytes.len() > MAX_MANIFEST_BYTES {
                    return Err(format!("plugin.toml exceeds {MAX_MANIFEST_BYTES} bytes"));
                }
                String::from_utf8(bytes).map_err(|error| error.to_string())
            })
            .and_then(|text| {
                toml::from_str::<PluginSpec>(&text).map_err(|error| error.to_string())
            });
        match parsed {
            Ok(spec) => out.push(LoadedPlugin {
                spec,
                dir: Some(dir),
            }),
            Err(e) => {
                tracing::warn!(target: "thegn::plugin", path = %manifest_path.display(), error = %e, "unparseable plugin.toml");
                problems.push(discovery_problem(
                    &dir,
                    format!("plugin.toml does not parse: {e}"),
                ));
            }
        }
    }
    problems
        .sort_by(|left, right| (&left.plugin, &left.problem).cmp(&(&right.plugin, &right.problem)));
    (out, problems)
}

fn discovery_problem(path: &Path, problem: String) -> SpecProblem {
    SpecProblem {
        plugin: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        problem,
    }
}

fn valid_routing_id(id: &str) -> bool {
    (MIN_PLUGIN_ROUTING_ID_BYTES..=MAX_PLUGIN_ROUTING_ID_BYTES).contains(&id.len())
        && !id
            .chars()
            .any(|character| character.is_control() || character == ':')
}

/// Negotiate one spec against the host contract.
pub fn negotiate(spec: &PluginSpec) -> Result<NegotiatedManifest, String> {
    host_contract()
        .negotiate_spec(spec)
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
    if !(MIN_PLUGIN_ID_BYTES..=MAX_PLUGIN_ID_BYTES).contains(&id.len())
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        push(format!(
            "plugin id must be {MIN_PLUGIN_ID_BYTES}..={MAX_PLUGIN_ID_BYTES} ASCII bytes using [A-Za-z0-9._-]"
        ));
    }
    if !(MIN_PLUGIN_TIMEOUT_SECS..=MAX_PLUGIN_TIMEOUT_SECS).contains(&p.spec.timeout_secs) {
        push(format!(
            "timeout_secs must be {MIN_PLUGIN_TIMEOUT_SECS}..={MAX_PLUGIN_TIMEOUT_SECS}"
        ));
    }
    for cap in &p.spec.manifest.capabilities {
        if cap.kind().is_empty()
            || cap.target().is_empty()
            || cap.as_str().chars().any(char::is_control)
        {
            push(format!("invalid capability {:?}", cap.as_str()));
        }
    }
    for contribution in &p.spec.manifest.contributions {
        if !valid_routing_id(contribution.id.as_str()) {
            push(format!(
                "contribution {:?}: invalid id",
                contribution.id.as_str()
            ));
        }
        if let Some(surface) = &contribution.surface
            && !valid_routing_id(surface.as_str())
        {
            push(format!(
                "contribution {}: invalid surface",
                contribution.id.as_str()
            ));
        }
        if let CadenceHint::Interval { millis } = &contribution.cadence
            && !(MIN_PLUGIN_INTERVAL_MILLIS..=MAX_PLUGIN_INTERVAL_MILLIS).contains(millis)
        {
            push(format!(
                "contribution {} interval millis must be {MIN_PLUGIN_INTERVAL_MILLIS}..={MAX_PLUGIN_INTERVAL_MILLIS}",
                contribution.id.as_str()
            ));
        }
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
            for rejected in &neg.rejected_contributions {
                push(format!(
                    "contribution {:?} rejected: {}",
                    rejected.contribution.id.as_str(),
                    rejected.reason
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
    let (loaded, broken) = discover(cfg, config_dir);
    check_loaded(&loaded, broken)
}

/// Validate one discovered snapshot (including parse failures from discovery).
pub fn check_loaded(loaded: &[LoadedPlugin], broken: Vec<SpecProblem>) -> Vec<SpecProblem> {
    let mut out = broken;
    let mut seen: HashSet<String> = HashSet::new();
    for p in loaded {
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
    use thegn_core::plugin_api::{ApiVersion, Contribution, ExtensionPoint, PluginManifest};

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
        let (loaded, problems) = discover(&cfg, tmp.path());
        assert!(problems.is_empty());
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
    fn check_spec_enforces_plugin_id_grammar() {
        let overlong = "x".repeat(MAX_PLUGIN_ID_BYTES + 1);
        for id in [
            "",
            "has:delimiter",
            "white space",
            "control\u{7}",
            "é",
            overlong.as_str(),
        ] {
            let loaded = LoadedPlugin {
                spec: spec(id, vec!["true".into()]),
                dir: None,
            };
            assert!(
                check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.contains("plugin id must be")),
                "id {id:?} should fail canonical grammar"
            );
        }
        let max_id = "x".repeat(MAX_PLUGIN_ID_BYTES);
        let loaded = LoadedPlugin {
            spec: spec(&max_id, vec!["true".into()]),
            dir: None,
        };
        assert!(
            !check_spec(&loaded)
                .iter()
                .any(|problem| problem.problem.contains("plugin id must be"))
        );
    }

    #[test]
    fn check_spec_bounds_timeout_and_interval_values() {
        for (value, valid) in [
            (0, false),
            (1, true),
            (3600, true),
            (3601, false),
            (u64::MAX, false),
        ] {
            let mut plugin = spec("bounded", vec!["true".into()]);
            plugin.timeout_secs = value;
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
            };
            assert_eq!(
                check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.contains("timeout_secs must be")),
                !valid,
                "timeout {value}"
            );
        }

        for (millis, valid) in [
            (99, false),
            (100, true),
            (86_400_000, true),
            (86_400_001, false),
            (u64::MAX, false),
        ] {
            let mut plugin = spec("bounded", vec!["true".into()]);
            plugin
                .manifest
                .capabilities
                .push(Capability::new("surface", "statusbar"));
            plugin
                .manifest
                .contributions
                .push(thegn_core::plugin_api::Contribution {
                    id: thegn_core::plugin_api::ContributionId::new("bounded.interval"),
                    extension_point: thegn_core::plugin_api::ExtensionPoint::StatusBarSegment,
                    label: "Bounded".into(),
                    surface: Some(thegn_core::plugin_api::SurfaceId::new("bounded.surface")),
                    cadence: CadenceHint::Interval { millis },
                    metadata: Default::default(),
                    caps: serde_json::Value::Null,
                    chord: None,
                });
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
            };
            assert_eq!(
                check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.contains("interval millis must be")),
                !valid,
                "interval {millis}"
            );
        }
    }

    #[test]
    fn check_spec_rejects_capabilities_with_empty_or_control_components() {
        for capability in [
            Capability::new("", "target"),
            Capability::new("kind", ""),
            Capability::new("a\u{7}", "b"),
        ] {
            let mut plugin = spec("caps", vec!["true".into()]);
            plugin.manifest.capabilities.push(capability);
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
            };
            assert!(
                check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.contains("invalid capability"))
            );
        }
    }

    #[test]
    fn disabled_specs_are_listed_but_not_check_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let mut s = spec("off", vec!["thegn-no-such-plugin-binary".into()]);
        s.enabled = false;
        cfg.plugins.push(s);
        assert_eq!(discover(&cfg, tmp.path()).0.len(), 1);
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
        let (loaded, problems) = discover(&cfg, tmp.path());
        assert!(loaded.is_empty());
        assert_eq!(problems.len(), 1);
        let problems = check_specs(&cfg, tmp.path());
        assert!(
            problems
                .iter()
                .any(|p| p.problem.contains("does not parse")),
            "{problems:?}"
        );
    }

    #[test]
    fn unreadable_plugins_path_is_reported_but_missing_path_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_path = tmp.path().join("plugins");
        std::fs::write(&plugins_path, "not a directory").unwrap();
        let (loaded, problems) = discover(&Config::default(), tmp.path());
        assert!(loaded.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("not a directory"));

        let missing = tempfile::tempdir().unwrap();
        assert!(discover(&Config::default(), missing.path()).1.is_empty());
    }

    #[test]
    fn manifest_size_limit_rejects_over_limit_and_accepts_exact_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugins/limited");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let spec_text = toml::to_string(&spec("limited", vec!["true".into()])).unwrap();
        let mut exact = format!("{spec_text}\n");
        assert!(exact.len() < MAX_MANIFEST_BYTES);
        exact.push_str(&format!(
            "#{}\n",
            "x".repeat(MAX_MANIFEST_BYTES - exact.len() - 2)
        ));
        assert_eq!(exact.len(), MAX_MANIFEST_BYTES);
        std::fs::write(plugin_dir.join("plugin.toml"), &exact).unwrap();
        let (loaded, problems) = discover(&Config::default(), tmp.path());
        assert_eq!(loaded.len(), 1);
        assert!(problems.is_empty());

        std::fs::write(plugin_dir.join("plugin.toml"), format!("{exact}x")).unwrap();
        let (loaded, problems) = discover(&Config::default(), tmp.path());
        assert!(loaded.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("exceeds"));
    }

    #[test]
    fn plugin_directory_count_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        for index in 0..=MAX_PLUGIN_DIRS {
            let plugin_dir = plugins_dir.join(format!("plugin-{index}"));
            std::fs::create_dir(&plugin_dir).unwrap();
            std::fs::write(plugin_dir.join("plugin.toml"), "").unwrap();
        }
        let (loaded, problems) = discover(&Config::default(), tmp.path());
        assert!(loaded.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("directory count exceeds"));
    }

    #[test]
    fn plugin_directory_count_ignores_directories_without_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        for index in 0..=MAX_PLUGIN_DIRS {
            std::fs::create_dir(plugins_dir.join(format!("empty-{index}"))).unwrap();
        }
        let (loaded, problems) = discover(&Config::default(), tmp.path());
        assert!(loaded.is_empty());
        assert!(problems.is_empty());
    }

    #[test]
    fn check_spec_rejects_noncanonical_contribution_and_surface_ids() {
        let make_contribution = |id: &str, surface: &str| Contribution {
            id: thegn_core::plugin_api::ContributionId::new(id),
            extension_point: ExtensionPoint::StatusBarSegment,
            label: "Segment".into(),
            surface: Some(thegn_core::plugin_api::SurfaceId::new(surface)),
            cadence: CadenceHint::OnDemand,
            metadata: Default::default(),
            caps: serde_json::Value::Null,
            chord: None,
        };
        let long_id = "x".repeat(MAX_PLUGIN_ROUTING_ID_BYTES + 1);
        for id in ["", "a:b", "a\u{7}", long_id.as_str()] {
            let mut plugin = spec("routing", vec!["true".into()]);
            plugin
                .manifest
                .contributions
                .push(make_contribution(id, "seg"));
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
            };
            assert!(
                check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.contains("invalid id")),
                "contribution id {id:?} should fail"
            );
        }
        let long_surface = "x".repeat(MAX_PLUGIN_ROUTING_ID_BYTES + 1);
        for surface in ["", "a:b", "a\u{7}", long_surface.as_str()] {
            let mut plugin = spec("routing", vec!["true".into()]);
            plugin
                .manifest
                .contributions
                .push(make_contribution("seg", surface));
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
            };
            assert!(
                check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.contains("invalid surface")),
                "surface id {surface:?} should fail"
            );
        }
        let mut plugin = spec("routing", vec!["true".into()]);
        plugin
            .manifest
            .capabilities
            .push(Capability::new("surface", "statusbar"));
        plugin
            .manifest
            .contributions
            .push(make_contribution("é", "n/alerts"));
        assert!(
            check_spec(&LoadedPlugin {
                spec: plugin,
                dir: None
            })
            .is_empty()
        );
    }

    #[test]
    fn canonical_support_table_drives_the_loader_and_public_docs() {
        let contract = host_contract();
        let developer_guide = include_str!("../../../../docs/extending/plugin.md");
        let help = include_str!("../../../../docs/help/plugins.md");
        for row in HOST_EXTENSION_SUPPORT {
            let name = row.extension_point.wire_name();
            assert!(
                developer_guide.contains(name),
                "developer guide omits {name}"
            );
            assert!(help.contains(name), "plugin help omits {name}");
            assert_eq!(
                contract
                    .available_extension_points
                    .contains(&row.extension_point),
                row.state == thegn_core::plugin_api::SupportState::Wired,
                "loader support differs from the canonical row for {name}"
            );
            if let Some(required) = row.required_capability {
                let expected_grant = HOST_EXTENSION_SUPPORT.iter().any(|candidate| {
                    candidate.state == SupportState::Wired
                        && candidate.required_capability == Some(required)
                });
                assert_eq!(
                    contract
                        .granted_capabilities
                        .contains(&Capability::parse(required).unwrap()),
                    expected_grant,
                    "loader grant differs from the canonical rows for {required} (observed at {name})"
                );
            }
            let state = match row.state {
                SupportState::Wired => "wired",
                SupportState::SeparateAdapter => "separate adapter",
                SupportState::Reserved => "reserved",
            };
            let documented_state = developer_guide.lines().find_map(|line| {
                let cells = line
                    .strip_prefix('|')?
                    .split('|')
                    .map(str::trim)
                    .collect::<Vec<_>>();
                (cells.len() > 1 && cells[0].trim_matches('`') == name).then_some(cells[1])
            });
            assert_eq!(
                documented_state,
                Some(state),
                "developer guide support state differs for {name}"
            );
        }
        assert!(developer_guide.contains("x-thegn-extension-support"));
        assert!(developer_guide.contains("x-thegn-host-verb-support"));
        assert!(developer_guide.contains("x-thegn-host-call-capabilities"));

        let example: PluginSpec = toml::from_str(include_str!(
            "../../../../examples/plugins/hello/plugin.toml"
        ))
        .unwrap();
        assert_eq!(example.manifest.api, API_VERSION);
        assert!(negotiate(&example).is_ok());
    }
}
