//! Plugin discovery + validation: `[[plugins]]` config entries merged with
//! `<config_dir>/plugins/<dir>/plugin.toml` directories, each checked against
//! the host contract before anything runs.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::plugin_api::{
    API_VERSION, CadenceHint, Capability, HOST_EXTENSION_SUPPORT, HostContract, NativeTrust,
    NegotiatedManifest, PluginSpec, SupportState,
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
    /// A `trusted_native = true` value from plugin.toml is ignored; only the
    /// user's `[[plugins]]` config entry can grant native trust.
    pub trusted_native_ignored: bool,
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

/// Admission decision for one discovered plugin specification.
#[derive(Debug, Clone)]
pub enum PluginDecision {
    Admitted(NegotiatedManifest),
    Disabled,
    RefusedUntrusted(SpecProblem),
    Invalid(Vec<SpecProblem>),
}

/// A plugin together with the exact source bytes and admission decision.
#[derive(Debug, Clone)]
pub struct AdmittedPlugin {
    pub plugin: LoadedPlugin,
    pub source_digest: String,
    pub decision: PluginDecision,
}

/// One immutable discovery and validation result shared by every plugin surface.
#[derive(Debug, Clone)]
pub struct AdmittedPluginSet {
    pub plugins: Vec<AdmittedPlugin>,
    pub set_problems: Vec<SpecProblem>,
    pub digest: String,
}

impl AdmittedPluginSet {
    /// Return negotiated plugins only when all set-wide and enabled-plugin
    /// validation passed. Disabled and untrusted plugins remain non-startable.
    pub fn startable(&self) -> Result<Vec<(&LoadedPlugin, &NegotiatedManifest)>, Vec<SpecProblem>> {
        let mut problems = self.set_problems.clone();
        for admitted in &self.plugins {
            if let PluginDecision::Invalid(invalid) = &admitted.decision {
                problems.extend(invalid.iter().cloned());
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(self
            .plugins
            .iter()
            .filter_map(|admitted| {
                if let PluginDecision::Admitted(manifest) = &admitted.decision {
                    Some((&admitted.plugin, manifest))
                } else {
                    None
                }
            })
            .collect())
    }

    /// All visible problems in deterministic set, plugin, then refusal order.
    pub fn problems(&self) -> Vec<SpecProblem> {
        let mut problems = self.set_problems.clone();
        for admitted in &self.plugins {
            if let PluginDecision::Invalid(invalid) = &admitted.decision {
                problems.extend(invalid.iter().cloned());
            }
        }
        for admitted in &self.plugins {
            if let PluginDecision::RefusedUntrusted(problem) = &admitted.decision {
                problems.push(problem.clone());
            }
        }
        problems
    }
}

/// Discover and admit a plugin set once. Every decision and digest refers to
/// the same manifest bytes read during discovery.
pub fn admit(cfg: &Config, config_dir: &Path) -> AdmittedPluginSet {
    let (loaded, broken, source_digests) = discover_with_digests(cfg, config_dir);
    let mut set_problems = broken;
    set_problems.extend(duplicate_id_problems(
        loaded.iter().map(|plugin| plugin.spec.manifest.id.as_str()),
    ));
    set_problems
        .sort_by(|left, right| (&left.plugin, &left.problem).cmp(&(&right.plugin, &right.problem)));

    let mut plugins = Vec::with_capacity(loaded.len());
    for (plugin, source_digest) in loaded.into_iter().zip(source_digests) {
        let id = plugin.spec.manifest.id.as_str().to_string();
        let decision = if !plugin.spec.enabled {
            PluginDecision::Disabled
        } else if let Some(problem) = native_trust_refusal(&plugin) {
            PluginDecision::RefusedUntrusted(problem)
        } else {
            let negotiated = negotiate(&plugin.spec);
            let mut problems = check_spec_fields(&plugin);
            match negotiated {
                Ok(manifest) => {
                    problems.extend(manifest.rejected_contributions.iter().map(|rejected| {
                        SpecProblem {
                            plugin: id.clone(),
                            problem: format!(
                                "contribution {:?} rejected: {}",
                                rejected.contribution.id.as_str(),
                                rejected.reason
                            ),
                        }
                    }));
                    if problems.is_empty() {
                        PluginDecision::Admitted(manifest)
                    } else {
                        PluginDecision::Invalid(problems)
                    }
                }
                Err(problem) => {
                    problems.push(SpecProblem {
                        plugin: id,
                        problem,
                    });
                    PluginDecision::Invalid(problems)
                }
            }
        };
        plugins.push(AdmittedPlugin {
            plugin,
            source_digest,
            decision,
        });
    }
    let mut digest_rows = plugins
        .iter()
        .map(|admitted| {
            let id = admitted.plugin.spec.manifest.id.as_str();
            format!(
                "{id}\t{}\t{}",
                admitted.source_digest,
                decision_kind(&admitted.decision)
            )
        })
        .collect::<Vec<_>>();
    digest_rows.sort();
    let digest = digest_bytes(digest_rows.join("\n").as_bytes());
    AdmittedPluginSet {
        plugins,
        set_problems,
        digest,
    }
}

fn decision_kind(decision: &PluginDecision) -> &'static str {
    match decision {
        PluginDecision::Admitted(_) => "admitted",
        PluginDecision::Disabled => "disabled",
        PluginDecision::RefusedUntrusted(_) => "refused-untrusted",
        PluginDecision::Invalid(_) => "invalid",
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
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
    let (plugins, problems, _) = discover_with_digests(cfg, config_dir);
    (plugins, problems)
}

fn discover_with_digests(
    cfg: &Config,
    config_dir: &Path,
) -> (Vec<LoadedPlugin>, Vec<SpecProblem>, Vec<String>) {
    let mut out: Vec<LoadedPlugin> = cfg
        .plugins
        .iter()
        .cloned()
        .map(|spec| LoadedPlugin {
            spec,
            dir: None,
            trusted_native_ignored: false,
        })
        .collect();
    let mut source_digests = Vec::new();
    let mut problems = Vec::new();
    for plugin in &out {
        match serde_json::to_vec(&plugin.spec) {
            Ok(encoded) => source_digests.push(digest_bytes(&encoded)),
            Err(error) => {
                problems.push(SpecProblem {
                    plugin: plugin.spec.manifest.id.as_str().to_string(),
                    problem: format!("could not digest [[plugins]] entry: {error}"),
                });
                source_digests.push(String::new());
            }
        }
    }
    let plugins_dir = config_dir.join("plugins");
    match std::fs::metadata(&plugins_dir) {
        Ok(metadata) if !metadata.is_dir() => {
            problems.push(discovery_problem(
                &plugins_dir,
                "plugins path is not a directory".into(),
            ));
            return (out, problems, source_digests);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (out, problems, source_digests);
        }
        Err(error) => {
            problems.push(discovery_problem(&plugins_dir, error.to_string()));
            return (out, problems, source_digests);
        }
    }

    let entries = match std::fs::read_dir(&plugins_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (out, problems, source_digests);
        }
        Err(error) => {
            problems.push(discovery_problem(&plugins_dir, error.to_string()));
            return (out, problems, source_digests);
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
        return (out, problems, source_digests);
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
                let source_digest = digest_bytes(&bytes);
                String::from_utf8(bytes)
                    .map(|text| (text, source_digest))
                    .map_err(|error| error.to_string())
            })
            .and_then(|(text, source_digest)| {
                toml::from_str::<PluginSpec>(&text)
                    .map(|spec| (spec, source_digest))
                    .map_err(|error| error.to_string())
            });
        match parsed {
            Ok((mut spec, source_digest)) => {
                let trusted_native_ignored = spec.trusted_native;
                spec.trusted_native = false;
                source_digests.push(source_digest);
                out.push(LoadedPlugin {
                    spec,
                    dir: Some(dir),
                    trusted_native_ignored,
                });
            }
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
    (out, problems, source_digests)
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

fn valid_env_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    (1..=MAX_PLUGIN_ROUTING_ID_BYTES).contains(&bytes.len())
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn launch_field_bytes(spec: &PluginSpec) -> usize {
    let command_bytes = spec.command.iter().fold(0_usize, |total, argument| {
        total.saturating_add(argument.len()).saturating_add(1)
    });
    let env_bytes = spec.env.iter().fold(0_usize, |total, (key, value)| {
        total
            .saturating_add(key.len())
            .saturating_add(value.len())
            .saturating_add(2)
    });
    command_bytes
        .saturating_add(spec.cwd.len())
        .saturating_add(1)
        .saturating_add(env_bytes)
}

fn duplicate_id_problems<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<SpecProblem> {
    let mut seen = HashMap::<String, String>::new();
    let mut problems = Vec::new();
    for id in ids {
        let canonical = id.to_ascii_lowercase();
        if let Some(first) = seen.get(&canonical) {
            problems.push(SpecProblem {
                plugin: id.to_string(),
                problem: if first.as_str() == id {
                    "duplicate plugin id".into()
                } else {
                    format!("duplicate plugin id (differs only by case from {first:?})")
                },
            });
        } else {
            seen.insert(canonical, id.to_string());
        }
    }
    problems
}

/// Negotiate one spec against the host contract.
pub fn negotiate(spec: &PluginSpec) -> Result<NegotiatedManifest, String> {
    host_contract()
        .negotiate_spec(spec)
        .map_err(|e| e.to_string())
}

/// Validate one loaded plugin; empty = clean.
pub fn check_spec(p: &LoadedPlugin) -> Vec<SpecProblem> {
    let mut out = check_spec_fields(p);
    match negotiate(&p.spec) {
        Err(e) => out.push(SpecProblem {
            plugin: p.spec.manifest.id.as_str().to_string(),
            problem: e,
        }),
        Ok(neg) => {
            for rejected in &neg.rejected_contributions {
                out.push(SpecProblem {
                    plugin: p.spec.manifest.id.as_str().to_string(),
                    problem: format!(
                        "contribution {:?} rejected: {}",
                        rejected.contribution.id.as_str(),
                        rejected.reason
                    ),
                });
            }
        }
    }
    out
}

fn check_spec_fields(p: &LoadedPlugin) -> Vec<SpecProblem> {
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
    for (index, argument) in p.spec.command.iter().enumerate() {
        if argument.contains('\0') {
            push(format!("command argument {index} contains a NUL byte"));
        }
    }
    if p.spec.cwd.contains('\0') {
        push("cwd contains a NUL byte".into());
    }
    for (key, value) in &p.spec.env {
        if !valid_env_key(key) {
            push(format!(
                "env key {key:?} must be 1..={MAX_PLUGIN_ROUTING_ID_BYTES} bytes of [A-Za-z0-9_], not starting with a digit"
            ));
        }
        if value.contains('\0') {
            push(format!("env {key} value contains a NUL byte"));
        }
    }
    let launch_bytes = launch_field_bytes(&p.spec);
    if launch_bytes > MAX_MANIFEST_BYTES {
        push(format!(
            "launch fields (command, cwd, env) are {launch_bytes} bytes; the limit is {MAX_MANIFEST_BYTES}"
        ));
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
    out.extend(duplicate_id_problems(
        loaded.iter().map(|plugin| plugin.spec.manifest.id.as_str()),
    ));
    for p in loaded.iter().filter(|p| p.spec.enabled) {
        out.extend(check_spec(p));
    }
    out
}

/// Refuse one enabled capability-gated plugin without rejecting its peers.
pub fn native_trust_refusal(plugin: &LoadedPlugin) -> Option<SpecProblem> {
    if !plugin.spec.enabled {
        return None;
    }
    let NativeTrust::Refused { gated_by } = plugin.spec.native_trust() else {
        return None;
    };
    let problem = if plugin.dir.is_some() {
        format!(
            "refused: declares {} but capabilities do not sandbox native plugin code; move this plugin directory out of plugins/ (or remove its plugin.toml), then declare it as a [[plugins]] entry with absolute command and cwd paths and trusted_native = true to run it with your full user authority",
            gated_by.join(", ")
        )
    } else {
        format!(
            "refused: declares {} but capabilities do not sandbox native plugin code; add trusted_native = true to its [[plugins]] entry to run it with your full user authority",
            gated_by.join(", ")
        )
    };
    Some(SpecProblem {
        plugin: plugin.spec.manifest.id.as_str().to_string(),
        problem,
    })
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
            trusted_native: false,
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
                trusted_native_ignored: false,
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
            trusted_native_ignored: false,
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
                trusted_native_ignored: false,
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
                trusted_native_ignored: false,
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
                trusted_native_ignored: false,
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
                trusted_native_ignored: false,
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
                trusted_native_ignored: false,
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
                dir: None,
                trusted_native_ignored: false,
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

    #[test]
    fn untrusted_gated_plugin_is_refused_with_the_config_line() {
        let mut plugin_spec = spec("untrusted", vec!["true".into()]);
        plugin_spec
            .manifest
            .capabilities
            .push(Capability::new("network", "example.com"));
        let plugin = LoadedPlugin {
            spec: plugin_spec,
            dir: None,
            trusted_native_ignored: false,
        };
        let problem = native_trust_refusal(&plugin).expect("gated plugin is refused");
        assert!(problem.to_string().contains("untrusted"));
        assert!(problem.problem.contains("network:example.com"));
        assert!(problem.problem.contains("trusted_native = true"));
        assert!(problem.problem.contains("its [[plugins]] entry"));
    }

    #[test]
    fn directory_plugin_cannot_trust_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins/self-trusting");
        std::fs::create_dir_all(&dir).unwrap();
        let mut plugin_spec = spec("self-trusting", vec!["true".into()]);
        plugin_spec
            .manifest
            .capabilities
            .push(Capability::new("network", "example.com"));
        plugin_spec.trusted_native = true;
        std::fs::write(
            dir.join("plugin.toml"),
            toml::to_string(&plugin_spec).unwrap(),
        )
        .unwrap();

        let (loaded, problems) = discover(&Config::default(), tmp.path());
        assert!(problems.is_empty());
        assert!(!loaded[0].spec.trusted_native);
        assert!(loaded[0].trusted_native_ignored);
        let problem = native_trust_refusal(&loaded[0]).expect("manifest trust is ignored");
        assert!(
            problem
                .problem
                .contains("move this plugin directory out of plugins/")
        );
        assert!(problem.problem.contains("remove its plugin.toml"));
        assert!(problem.problem.contains("absolute command and cwd paths"));
    }

    #[test]
    fn refusal_does_not_reject_the_rest_of_the_set() {
        let mut gated = spec("untrusted", vec!["true".into()]);
        gated
            .manifest
            .capabilities
            .push(Capability::new("network", "example.com"));
        let surface: PluginSpec = toml::from_str(include_str!(
            "../../../../examples/plugins/hello/plugin.toml"
        ))
        .unwrap();
        let loaded = [
            LoadedPlugin {
                spec: gated,
                dir: None,
                trusted_native_ignored: false,
            },
            LoadedPlugin {
                spec: surface,
                dir: None,
                trusted_native_ignored: false,
            },
        ];
        assert!(check_loaded(&loaded, Vec::new()).is_empty());
        assert!(native_trust_refusal(&loaded[0]).is_some());
        assert!(native_trust_refusal(&loaded[1]).is_none());
    }

    #[test]
    fn admit_marks_a_negotiate_failure_invalid_and_blocks_startable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut invalid = spec("future", vec!["true".into()]);
        invalid.manifest.api = ApiVersion::new(9, 0, 0);
        let cfg = Config {
            plugins: vec![invalid],
            ..Default::default()
        };

        let admitted = admit(&cfg, tmp.path());
        let PluginDecision::Invalid(problems) = &admitted.plugins[0].decision else {
            panic!("incompatible api must be invalid");
        };
        assert!(
            problems
                .iter()
                .any(|problem| problem.problem.contains("incompatible api"))
        );
        assert_eq!(admitted.startable().unwrap_err(), admitted.problems());
    }

    #[test]
    fn admit_records_stable_digests_that_change_with_manifest_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins/digest");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("plugin.toml");
        let source = toml::to_string(&spec("digest", vec!["true".into()])).unwrap();
        std::fs::write(&manifest, &source).unwrap();

        let first = admit(&Config::default(), tmp.path());
        let same = admit(&Config::default(), tmp.path());
        assert_eq!(
            first.plugins[0].source_digest,
            same.plugins[0].source_digest
        );
        assert_eq!(first.digest, same.digest);

        std::fs::write(&manifest, format!("{source}\n")).unwrap();
        let changed = admit(&Config::default(), tmp.path());
        assert_ne!(
            first.plugins[0].source_digest,
            changed.plugins[0].source_digest
        );
        assert_ne!(first.digest, changed.digest);
    }

    // Positive control: exact duplicate ids retain the established diagnostic.
    #[test]
    fn admit_set_problems_include_duplicate_ids_and_discovery_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_dir = tmp.path().join("plugins/broken");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("plugin.toml"), "not = [valid").unwrap();
        let cfg = Config {
            plugins: vec![
                spec("same", vec!["true".into()]),
                spec("same", vec!["true".into()]),
            ],
            ..Default::default()
        };

        let admitted = admit(&cfg, tmp.path());
        assert!(
            admitted
                .set_problems
                .iter()
                .any(|problem| problem.problem == "duplicate plugin id")
        );
        assert!(
            admitted
                .set_problems
                .iter()
                .any(|problem| problem.problem.contains("plugin.toml does not parse"))
        );
        assert_eq!(admitted.problems(), admitted.startable().unwrap_err());
    }

    #[test]
    fn admit_refuses_a_plugin_whose_contributions_share_a_surface() {
        use thegn_core::plugin_api::{ContributionId, PluginMode, SurfaceId};

        let tmp = tempfile::tempdir().unwrap();
        let mut duplicate_surface = spec("dupsurf", vec!["true".into()]);
        duplicate_surface.mode = PluginMode::Resident;
        duplicate_surface.manifest.capabilities = vec![Capability::new("surface", "statusbar")];
        for id in ["dupsurf.a", "dupsurf.b"] {
            duplicate_surface.manifest.contributions.push(Contribution {
                id: ContributionId::new(id),
                extension_point: ExtensionPoint::StatusBarSegment,
                label: id.into(),
                surface: Some(SurfaceId::new("dupsurf.seg")),
                cadence: CadenceHint::OnDemand,
                metadata: Default::default(),
                caps: serde_json::Value::Null,
                chord: None,
            });
        }

        let admitted = admit(
            &Config {
                plugins: vec![duplicate_surface],
                ..Default::default()
            },
            tmp.path(),
        );
        let plugin = admitted
            .plugins
            .iter()
            .find(|plugin| plugin.plugin.spec.manifest.id.as_str() == "dupsurf")
            .unwrap();
        match &plugin.decision {
            PluginDecision::Invalid(problems) => assert!(
                problems
                    .iter()
                    .any(|problem| problem.problem.contains("duplicate surface dupsurf.seg"))
            ),
            decision => panic!("expected invalid plugin decision, got {decision:?}"),
        }
        assert!(admitted.startable().is_err());
    }

    #[test]
    fn launch_fields_with_nul_bytes_are_refused() {
        let cases = [
            (
                spec("argv-nul", vec!["true".into(), "a\0b".into()]),
                "command argument 1 contains a NUL byte",
            ),
            (
                {
                    let mut plugin = spec("cwd-nul", vec!["true".into()]);
                    plugin.cwd = "x\0y".into();
                    plugin
                },
                "cwd contains a NUL byte",
            ),
            (
                {
                    let mut plugin = spec("env-nul", vec!["true".into()]);
                    plugin.env.insert("KEY".into(), "v\0".into());
                    plugin
                },
                "env KEY value contains a NUL byte",
            ),
            (
                {
                    let mut plugin = spec("env-key-nul", vec!["true".into()]);
                    plugin.env.insert("KE\0Y".into(), "value".into());
                    plugin
                },
                "env key \"KE\\0Y\" must be 1..=128 bytes of [A-Za-z0-9_], not starting with a digit",
            ),
        ];

        for (plugin, expected_problem) in cases {
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
                trusted_native_ignored: false,
            };
            let problems = check_spec(&loaded);
            assert_eq!(problems.len(), 1, "{problems:?}");
            assert_eq!(problems[0].problem, expected_problem);
        }

        let tmp = tempfile::tempdir().unwrap();
        let mut env_nul = spec("env-nul", vec!["true".into()]);
        env_nul.env.insert("KEY".into(), "v\0".into());
        let admitted = admit(
            &Config {
                plugins: vec![env_nul],
                ..Default::default()
            },
            tmp.path(),
        );
        assert!(matches!(
            &admitted.plugins[0].decision,
            PluginDecision::Invalid(problems)
                if problems.iter().any(|problem| problem.problem == "env KEY value contains a NUL byte")
        ));
    }

    #[test]
    fn env_key_grammar_is_enforced() {
        let invalid = [
            "".to_string(),
            "1A".to_string(),
            "A=B".to_string(),
            "A B".to_string(),
            "é".to_string(),
            "A\u{7}".to_string(),
            "K".repeat(MAX_PLUGIN_ROUTING_ID_BYTES + 1),
        ];
        for key in invalid {
            let mut plugin = spec("env-key", vec!["true".into()]);
            plugin.env.insert(key.clone(), "value".into());
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
                trusted_native_ignored: false,
            };
            assert!(
                check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.starts_with("env key ")),
                "key {key:?} should fail the env key grammar"
            );
        }

        for key in [
            "A".to_string(),
            "_X1".to_string(),
            "PATH".to_string(),
            "lower_case".to_string(),
            "K".repeat(MAX_PLUGIN_ROUTING_ID_BYTES),
        ] {
            let mut plugin = spec("env-key", vec!["true".into()]);
            plugin.env.insert(key.clone(), "value".into());
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
                trusted_native_ignored: false,
            };
            assert!(
                !check_spec(&loaded)
                    .iter()
                    .any(|problem| problem.problem.starts_with("env key ")),
                "key {key:?} should pass the env key grammar"
            );
        }
    }

    #[test]
    fn launch_field_budget_boundaries() {
        let base = spec("budget", vec!["true".into()]);
        let base_bytes = launch_field_bytes(&base);
        let exact_padding_len = MAX_MANIFEST_BYTES - base_bytes - 1;

        for (padding_len, expected_bytes, refused) in [
            (exact_padding_len - 1, MAX_MANIFEST_BYTES - 1, false),
            (exact_padding_len, MAX_MANIFEST_BYTES, false),
            (exact_padding_len + 1, MAX_MANIFEST_BYTES + 1, true),
        ] {
            let mut plugin = base.clone();
            plugin.command.push("x".repeat(padding_len));
            let actual_bytes = launch_field_bytes(&plugin);
            assert_eq!(
                actual_bytes, expected_bytes,
                "computed launch bytes: {actual_bytes}"
            );
            let loaded = LoadedPlugin {
                spec: plugin,
                dir: None,
                trusted_native_ignored: false,
            };
            let problems = check_spec(&loaded);
            let budget_problem = problems.iter().find(|problem| {
                problem
                    .problem
                    .starts_with("launch fields (command, cwd, env)")
            });
            assert_eq!(
                budget_problem.is_some(),
                refused,
                "computed launch bytes: {actual_bytes}; problems: {problems:?}"
            );
            if refused {
                let expected_problem = format!(
                    "launch fields (command, cwd, env) are {} bytes; the limit is {MAX_MANIFEST_BYTES}",
                    MAX_MANIFEST_BYTES + 1
                );
                assert_eq!(
                    budget_problem.map(|problem| problem.problem.as_str()),
                    Some(expected_problem.as_str()),
                    "computed launch bytes: {actual_bytes}"
                );
            }
        }
    }

    #[test]
    fn case_only_duplicate_ids_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            plugins: vec![
                spec("Hello", vec!["true".into()]),
                spec("hello", vec!["true".into()]),
            ],
            ..Default::default()
        };
        let expected = "duplicate plugin id (differs only by case from \"Hello\")";

        for problems in [
            check_specs(&cfg, tmp.path()),
            admit(&cfg, tmp.path()).set_problems,
        ] {
            assert!(
                problems
                    .iter()
                    .any(|problem| { problem.plugin == "hello" && problem.problem == expected }),
                "case-only duplicate diagnostic missing: {problems:?}"
            );
        }
        assert!(admit(&cfg, tmp.path()).startable().is_err());
    }
}
