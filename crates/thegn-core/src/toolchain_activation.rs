//! Substrate-free toolchain activation values and composition policy.
//!
//! Providers resolve their own environments outside core. This module only
//! detects declaration names, defines the synchronous provider seam, derives
//! cache/trust identities, and composes already-resolved values. A path entry
//! is always data here; no shell command or vendor process crosses this seam.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bundle::{ResolvedEnv, is_credential_key};
use crate::config_resolve::GatedRequest;
use crate::devenv::Devshell;

const CONFIG_FILES: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise.local.toml",
    "mise/config.toml",
    ".mise/config.toml",
    ".config/mise.toml",
    ".config/mise/config.toml",
];

const PIN_FILES: &[&str] = &[
    ".tool-versions",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".ruby-version",
    ".go-version",
    ".java-version",
];

/// Deterministic relative names of the toolchain declarations in a worktree.
/// Config files are separated from version pin files so callers can report the
/// distinction without reinterpreting paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedToolchainFiles {
    config_files: Vec<String>,
    pin_files: Vec<String>,
}

impl DetectedToolchainFiles {
    /// Normalize explicit names. Unsafe/unknown names are ignored and both
    /// groups are sorted and deduplicated.
    pub fn new<I, J>(config_files: I, pin_files: J) -> Self
    where
        I: IntoIterator<Item = String>,
        J: IntoIterator<Item = String>,
    {
        let config_files = config_files
            .into_iter()
            .filter(|path| is_config_name(path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let pin_files = pin_files
            .into_iter()
            .filter(|path| PIN_FILES.contains(&path.as_str()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self {
            config_files,
            pin_files,
        }
    }

    /// Readable, regular declaration files beneath `worktree`. Symlinks are
    /// ignored (including in-tree links) so the local and POSIX remote detector
    /// share a simple fail-closed rule that necessarily excludes outside links.
    pub fn detect(worktree: &Path, mise_env: Option<&str>) -> Self {
        let mut config_files = Vec::new();
        let mut pin_files = Vec::new();

        for path in CONFIG_FILES {
            if readable_regular_file(worktree, path).is_some() {
                config_files.push((*path).to_string());
            }
        }

        let conf_dir = worktree.join("conf.d");
        if !is_symlink(&conf_dir)
            && let Ok(entries) = std::fs::read_dir(&conf_dir)
        {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let relative = format!("conf.d/{name}");
                if is_config_name(&relative) && readable_regular_file(worktree, &relative).is_some()
                {
                    config_files.push(relative);
                }
            }
        }

        if let Some(env) = mise_env.filter(|env| safe_env_name(env)) {
            let relative = format!("mise.{env}.toml");
            if readable_regular_file(worktree, &relative).is_some() {
                config_files.push(relative);
            }
        }

        for path in PIN_FILES {
            if readable_regular_file(worktree, path).is_some() {
                pin_files.push((*path).to_string());
            }
        }

        Self::new(config_files, pin_files)
    }

    /// Parse the name-only lines emitted by `envplan::DETECT_PROBE_SCRIPT`.
    /// Malformed, absolute, traversal, and unknown names are ignored.
    pub fn from_probe(out: &str) -> Self {
        let mut config_files = Vec::new();
        let mut pin_files = Vec::new();
        for line in out.lines() {
            let Some((kind, path)) = line.trim().split_once('=') else {
                continue;
            };
            match kind {
                "TOOLCHAIN_CONFIG" if is_config_name(path) => {
                    config_files.push(path.to_string());
                }
                "TOOLCHAIN_PIN" if PIN_FILES.contains(&path) => {
                    pin_files.push(path.to_string());
                }
                _ => {}
            }
        }
        Self::new(config_files, pin_files)
    }

    pub fn config_files(&self) -> &[String] {
        &self.config_files
    }

    pub fn pin_files(&self) -> &[String] {
        &self.pin_files
    }

    pub fn is_empty(&self) -> bool {
        self.config_files.is_empty() && self.pin_files.is_empty()
    }

    /// All detected names in stable lexical order.
    pub fn all_files(&self) -> Vec<String> {
        self.config_files
            .iter()
            .chain(&self.pin_files)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

fn safe_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn is_config_name(path: &str) -> bool {
    if CONFIG_FILES.contains(&path) {
        return true;
    }
    if let Some(name) = path.strip_prefix("conf.d/") {
        return safe_leaf_toml(name);
    }
    path.strip_prefix("mise.")
        .and_then(|name| name.strip_suffix(".toml"))
        .is_some_and(safe_env_name)
}

fn safe_leaf_toml(name: &str) -> bool {
    !name.is_empty()
        && name.ends_with(".toml")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn readable_regular_file(worktree: &Path, relative: &str) -> Option<Vec<u8>> {
    if !safe_relative(relative) {
        return None;
    }
    let mut component = worktree.to_path_buf();
    for part in Path::new(relative).components() {
        let Component::Normal(part) = part else {
            return None;
        };
        component.push(part);
        if is_symlink(&component) {
            return None;
        }
    }
    let path = worktree.join(relative);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return None;
    }
    std::fs::read(path).ok()
}

/// Stable identity of exactly the declaration bytes a provider may evaluate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSetIdentity {
    pub hash: String,
    pub files: Vec<String>,
}

/// Hash the worktree identity plus every detected declaration's relative name
/// and bytes. `mise.lock` participates when readable. If a detected file races
/// with this read and disappears, return `None` so trust fails closed.
pub fn config_set_identity(
    worktree_identity: &str,
    worktree: &Path,
    detected: &DetectedToolchainFiles,
) -> Option<ConfigSetIdentity> {
    let mut contents = detected
        .all_files()
        .into_iter()
        .map(|relative| {
            let bytes = readable_regular_file(worktree, &relative)?;
            Some((relative, bytes))
        })
        .collect::<Option<Vec<_>>>()?;
    if readable_regular_file(worktree, "mise.lock").is_some() {
        contents.push((
            "mise.lock".to_string(),
            readable_regular_file(worktree, "mise.lock")?,
        ));
    }

    config_set_identity_from_bytes(worktree_identity, &contents)
}

/// Build the same identity as [`config_set_identity`] from declaration bytes
/// obtained through a provider target. The host adapter uses this when the
/// worktree is not present on the local filesystem; keeping the hash format in
/// core prevents remote trust requests from becoming a second identity scheme.
pub fn config_set_identity_from_bytes(
    worktree_identity: &str,
    contents: &[(String, Vec<u8>)],
) -> Option<ConfigSetIdentity> {
    if contents.is_empty() {
        return None;
    }
    let mut contents = contents.to_vec();
    contents.sort_by(|a, b| a.0.cmp(&b.0));
    if contents
        .windows(2)
        .any(|pair| pair[0].0 == pair[1].0 || !safe_relative(&pair[0].0))
        || !safe_relative(&contents.last()?.0)
    {
        return None;
    }

    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"thegn-toolchain-config-v1");
    hash_part(&mut hasher, worktree_identity.as_bytes());
    for (relative, bytes) in &contents {
        hash_part(&mut hasher, relative.as_bytes());
        hash_part(&mut hasher, bytes);
    }
    let hash = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(ConfigSetIdentity {
        hash,
        files: contents.into_iter().map(|(name, _)| name).collect(),
    })
}

fn hash_part(hasher: &mut Sha256, part: &[u8]) {
    hasher.update((part.len() as u64).to_be_bytes());
    hasher.update(part);
}

/// The canonical repo-trust request permitting environment resolution for one
/// exact config set. Only the digest and relative names enter the request.
pub fn mise_env_request(identity: &ConfigSetIdentity) -> GatedRequest {
    GatedRequest {
        key: "mise.env".to_string(),
        value: serde_json::json!({
            "hash": identity.hash,
            "files": identity.files,
        }),
        summary: format!(
            "resolve toolchain environment from {}",
            identity.files.join(", ")
        ),
    }
}

/// Provider-independent activation intent after config/trust policy resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationPolicy {
    Shims,
    Environment,
    Off,
}

/// Explicit, substrate-free context supplied to a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderContext {
    pub worktree_identity: String,
    pub detected: DetectedToolchainFiles,
    pub policy: ActivationPolicy,
    pub config_approved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderState {
    Ready,
    Unavailable,
    Reserved,
}

/// Reportable provider state. Reasons must describe degradation, never contain
/// resolved environment values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub origin: String,
    pub state: ProviderState,
    pub reason: Option<String>,
}

/// One ordered activation layer. `path_entries` are already individual paths,
/// and `env` contains only non-PATH pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationLayer {
    pub path_entries: Vec<String>,
    pub env: Vec<(String, String)>,
    pub origin: String,
    pub status: ProviderState,
}

impl ActivationLayer {
    pub fn ready(
        origin: impl Into<String>,
        path_entries: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            path_entries,
            env,
            origin: origin.into(),
            status: ProviderState::Ready,
        }
    }
}

/// The three normal outcomes of provider activation. Reserved is an ordinary
/// no-op for a context the implementation does not own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAnswer {
    Ready(ActivationLayer),
    Unavailable { origin: String, reason: String },
    Reserved { origin: String, reason: String },
}

impl ProviderAnswer {
    pub fn status(&self) -> ProviderStatus {
        match self {
            Self::Ready(layer) => ProviderStatus {
                origin: layer.origin.clone(),
                state: ProviderState::Ready,
                reason: None,
            },
            Self::Unavailable { origin, reason } => ProviderStatus {
                origin: origin.clone(),
                state: ProviderState::Unavailable,
                reason: Some(reason.clone()),
            },
            Self::Reserved { origin, reason } => ProviderStatus {
                origin: origin.clone(),
                state: ProviderState::Reserved,
                reason: Some(reason.clone()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProbe {
    pub origin: String,
    pub status: ProviderState,
    pub version: Option<String>,
    pub reason: Option<String>,
}

/// Synchronous policy seam. Implementations may use injected runners, but core
/// receives only explicit values and never owns their process/runtime types.
pub trait ToolchainProvider: Send + Sync {
    fn kind(&self) -> &'static str;
    fn probe(&self, context: &ProviderContext) -> ProviderProbe;
    fn activate(&self, context: &ProviderContext) -> ProviderAnswer;
}

/// Final high-to-low-precedence activation layers and every provider status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivationPlan {
    pub layers: Vec<ActivationLayer>,
    pub statuses: Vec<ProviderStatus>,
}

impl ActivationPlan {
    pub fn path_entries(&self) -> Vec<String> {
        self.layers
            .iter()
            .flat_map(|layer| layer.path_entries.iter().cloned())
            .collect()
    }

    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.layers
            .iter()
            .flat_map(|layer| layer.env.iter().cloned())
            .collect()
    }

    pub fn path(&self) -> Option<String> {
        let paths = self.path_entries();
        if paths.is_empty() {
            return None;
        }
        std::env::join_paths(paths.iter().map(Path::new))
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    }
}

/// Compose resolved values with fixed precedence: bundle PATH, devshell PATH,
/// provider PATHs (mise shims/`_.path`), then base PATH. Environment values are
/// fill-only in the same order; credential-shaped provider values are dropped.
pub fn compose_activation(
    bundle: &ResolvedEnv,
    devshell: Option<&Devshell>,
    provider_answers: &[ProviderAnswer],
    base_path: Option<&str>,
) -> ActivationPlan {
    let mut plan = ActivationPlan::default();
    let mut seen_keys = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();

    let mut bundle_env = BTreeMap::new();
    let mut bundle_paths = Vec::new();
    for (key, value) in &bundle.overrides {
        if key == "PATH" {
            append_path_value(&mut bundle_paths, value);
        } else {
            bundle_env.insert(key.clone(), value.clone());
        }
    }
    let bundle_env = fill_env(bundle_env, &mut seen_keys, false);
    dedup_paths(&mut bundle_paths, &mut seen_paths);
    if !bundle_paths.is_empty() || !bundle_env.is_empty() {
        plan.layers
            .push(ActivationLayer::ready("bundle", bundle_paths, bundle_env));
    }

    if let Some(devshell) = devshell {
        let mut paths = Vec::new();
        if let Some(path) = &devshell.path {
            append_path_value(&mut paths, path);
        }
        dedup_paths(&mut paths, &mut seen_paths);
        let env = fill_env(devshell.vars.iter().cloned(), &mut seen_keys, false);
        if !paths.is_empty() || !env.is_empty() {
            plan.layers
                .push(ActivationLayer::ready("devshell", paths, env));
        }
    }

    for answer in provider_answers {
        plan.statuses.push(answer.status());
        let ProviderAnswer::Ready(layer) = answer else {
            continue;
        };
        let mut paths = layer.path_entries.clone();
        dedup_paths(&mut paths, &mut seen_paths);
        let env = fill_env(
            layer.env.iter().filter(|(key, _)| key != "PATH").cloned(),
            &mut seen_keys,
            true,
        );
        plan.layers.push(ActivationLayer {
            path_entries: paths,
            env,
            origin: layer.origin.clone(),
            status: ProviderState::Ready,
        });
    }

    if let Some(base_path) = base_path {
        let mut paths = Vec::new();
        append_path_value(&mut paths, base_path);
        dedup_paths(&mut paths, &mut seen_paths);
        if !paths.is_empty() {
            plan.layers
                .push(ActivationLayer::ready("base", paths, Vec::new()));
        }
    }
    plan
}

fn append_path_value(out: &mut Vec<String>, value: &str) {
    out.extend(
        std::env::split_paths(value)
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.to_string_lossy().into_owned()),
    );
}

fn dedup_paths(paths: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    paths.retain(|path| !path.is_empty() && seen.insert(path.clone()));
}

fn fill_env<I>(
    values: I,
    seen: &mut BTreeSet<String>,
    filter_credentials: bool,
) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let sorted = values.into_iter().collect::<BTreeMap<_, _>>();
    sorted
        .into_iter()
        .filter(|(key, _)| {
            key != "PATH"
                && (!filter_credentials || !is_credential_key(key))
                && seen.insert(key.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_resolve::Approvals;

    fn temp(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("thegn-toolchain-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn write(root: &Path, relative: &str, body: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn detects_every_config_pin_env_and_sorted_conf_file() {
        let dir = temp("detect");
        for name in CONFIG_FILES {
            write(dir.path(), name, name);
        }
        write(dir.path(), "conf.d/20-z.toml", "z");
        write(dir.path(), "conf.d/01-a.toml", "a");
        write(dir.path(), "conf.d/not-toml.txt", "no");
        write(dir.path(), "mise.ci.toml", "ci");
        for name in PIN_FILES {
            write(dir.path(), name, name);
        }
        let got = DetectedToolchainFiles::detect(dir.path(), Some("ci"));
        assert_eq!(
            got.config_files(),
            &[
                ".config/mise.toml",
                ".config/mise/config.toml",
                ".mise.toml",
                ".mise/config.toml",
                "conf.d/01-a.toml",
                "conf.d/20-z.toml",
                "mise.ci.toml",
                "mise.local.toml",
                "mise.toml",
                "mise/config.toml",
            ]
        );
        assert_eq!(
            got.pin_files(),
            &[
                ".go-version",
                ".java-version",
                ".node-version",
                ".nvmrc",
                ".python-version",
                ".ruby-version",
                ".tool-versions",
            ]
        );
    }

    #[test]
    fn probe_normalizes_and_ignores_malformed_or_traversing_lines() {
        let got = DetectedToolchainFiles::from_probe(
            "TOOLCHAIN_CONFIG=mise.toml\n\
             TOOLCHAIN_CONFIG=conf.d/z.toml\n\
             TOOLCHAIN_CONFIG=conf.d/a.toml\n\
             TOOLCHAIN_PIN=.node-version\n\
             TOOLCHAIN_CONFIG=../mise.toml\n\
             TOOLCHAIN_CONFIG=/tmp/mise.toml\n\
             TOOLCHAIN_PIN=.unknown-version\n\
             TOOLCHAIN_CONFIG=conf.d/nested/bad.toml\n\
             TOOLCHAIN_CONFIG\n\
             NOPE=mise.local.toml\n",
        );
        assert_eq!(
            got.config_files(),
            &["conf.d/a.toml", "conf.d/z.toml", "mise.toml"]
        );
        assert_eq!(got.pin_files(), &[".node-version"]);
    }

    #[cfg(unix)]
    #[test]
    fn detection_ignores_symlinks_even_when_the_target_is_readable() {
        use std::os::unix::fs::symlink;

        let dir = temp("links");
        let outside = temp("outside");
        write(outside.path(), "mise.toml", "outside");
        symlink(
            outside.path().join("mise.toml"),
            dir.path().join("mise.toml"),
        )
        .unwrap();
        assert!(DetectedToolchainFiles::detect(dir.path(), None).is_empty());

        let nested = temp("nested-links");
        write(outside.path(), "mise.toml", "outside-nested");
        symlink(outside.path(), nested.path().join(".config")).unwrap();
        assert!(DetectedToolchainFiles::detect(nested.path(), None).is_empty());
    }

    #[test]
    fn malformed_mise_env_cannot_select_a_path() {
        let dir = temp("env-name");
        write(dir.path(), "mise.ci.toml", "ok");
        assert!(DetectedToolchainFiles::detect(dir.path(), Some("../ci")).is_empty());
        assert_eq!(
            DetectedToolchainFiles::detect(dir.path(), Some("ci")).config_files(),
            &["mise.ci.toml"]
        );
    }

    #[test]
    fn config_and_lock_edits_invalidate_identity_and_trust() {
        let dir = temp("identity");
        write(dir.path(), "mise.toml", "[tools]\nnode='20'\n");
        let detected = DetectedToolchainFiles::detect(dir.path(), None);
        let first = config_set_identity("repo/worktree", dir.path(), &detected).unwrap();
        let first_req = mise_env_request(&first);
        let approvals = Approvals::from_canonical([first_req.canonical()]);
        assert!(approvals.is_approved(&first_req));

        write(dir.path(), "mise.toml", "[tools]\nnode='22'\n");
        let config_edit = config_set_identity("repo/worktree", dir.path(), &detected).unwrap();
        assert_ne!(first.hash, config_edit.hash);
        assert!(!approvals.is_approved(&mise_env_request(&config_edit)));

        write(dir.path(), "mise.lock", "lock-v1");
        let lock_one = config_set_identity("repo/worktree", dir.path(), &detected).unwrap();
        write(dir.path(), "mise.lock", "lock-v2");
        let lock_two = config_set_identity("repo/worktree", dir.path(), &detected).unwrap();
        assert_ne!(config_edit.hash, lock_one.hash);
        assert_ne!(lock_one.hash, lock_two.hash);
        assert!(lock_two.files.contains(&"mise.lock".to_string()));
    }

    #[test]
    fn target_bytes_identity_matches_local_identity_and_rejects_unsafe_names() {
        let dir = temp("target-identity");
        write(dir.path(), "mise.toml", "[tools]\nnode='20'\n");
        write(dir.path(), ".tool-versions", "node 20\n");
        let detected = DetectedToolchainFiles::detect(dir.path(), None);
        let local = config_set_identity("repo/worktree", dir.path(), &detected).unwrap();
        let contents = detected
            .all_files()
            .into_iter()
            .map(|name| {
                let bytes = std::fs::read(dir.path().join(&name)).unwrap();
                (name, bytes)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            config_set_identity_from_bytes("repo/worktree", &contents),
            Some(local)
        );
        assert!(
            config_set_identity_from_bytes(
                "repo/worktree",
                &[("../mise.toml".into(), b"unsafe".to_vec())]
            )
            .is_none()
        );
    }

    #[test]
    fn trust_request_is_canonical_and_contains_names_not_contents() {
        let identity = ConfigSetIdentity {
            hash: "abc123".into(),
            files: vec![".tool-versions".into(), "mise.toml".into()],
        };
        let req = mise_env_request(&identity);
        assert_eq!(req.key, "mise.env");
        assert_eq!(
            req.canonical(),
            r#"{"key":"mise.env","value":{"files":[".tool-versions","mise.toml"],"hash":"abc123"}}"#
        );
        assert!(!req.canonical().contains("TOKEN"));
    }

    struct FakeProvider;

    impl ToolchainProvider for FakeProvider {
        fn kind(&self) -> &'static str {
            "fake"
        }

        fn probe(&self, context: &ProviderContext) -> ProviderProbe {
            ProviderProbe {
                origin: self.kind().into(),
                status: if context.policy == ActivationPolicy::Off {
                    ProviderState::Reserved
                } else {
                    ProviderState::Ready
                },
                version: Some("1".into()),
                reason: None,
            }
        }

        fn activate(&self, context: &ProviderContext) -> ProviderAnswer {
            if context.policy == ActivationPolicy::Off {
                ProviderAnswer::Reserved {
                    origin: self.kind().into(),
                    reason: "disabled".into(),
                }
            } else {
                ProviderAnswer::Ready(ActivationLayer::ready(
                    self.kind(),
                    vec!["/fake/shims".into()],
                    Vec::new(),
                ))
            }
        }
    }

    #[test]
    fn trait_is_object_safe_and_reserved_is_reported_as_a_noop() {
        let provider: Box<dyn ToolchainProvider> = Box::new(FakeProvider);
        let context = ProviderContext {
            worktree_identity: "wt".into(),
            detected: DetectedToolchainFiles::default(),
            policy: ActivationPolicy::Off,
            config_approved: false,
        };
        assert_eq!(provider.kind(), "fake");
        assert_eq!(provider.probe(&context).status, ProviderState::Reserved);
        let answer = provider.activate(&context);
        assert!(matches!(answer, ProviderAnswer::Reserved { .. }));
        let plan = compose_activation(&ResolvedEnv::default(), None, &[answer], Some("/bin"));
        assert_eq!(plan.path_entries(), vec!["/bin"]);
        assert_eq!(plan.statuses[0].state, ProviderState::Reserved);
    }

    #[test]
    fn composition_pins_path_and_fill_only_environment_precedence() {
        let bundle = ResolvedEnv {
            overrides: vec![
                ("PATH".into(), "/bundle/bin:/shared".into()),
                ("FOO".into(), "bundle".into()),
                ("ZED".into(), "bundle-z".into()),
            ],
            ..Default::default()
        };
        let devshell = Devshell {
            path: Some("/nix/bin:/shared".into()),
            vars: vec![
                ("BAR".into(), "nix".into()),
                ("FOO".into(), "nix-loses".into()),
            ],
        };
        let provider = ProviderAnswer::Ready(ActivationLayer::ready(
            "mise",
            vec!["/mise/bin".into(), "/mise/shims".into(), "/shared".into()],
            vec![
                ("BAR".into(), "mise-loses".into()),
                ("BAZ".into(), "mise".into()),
                ("PATH".into(), "/evil".into()),
            ],
        ));
        let plan = compose_activation(
            &bundle,
            Some(&devshell),
            &[provider],
            Some("/usr/bin:/bin:/shared"),
        );
        assert_eq!(
            plan.path_entries(),
            vec![
                "/bundle/bin",
                "/shared",
                "/nix/bin",
                "/mise/bin",
                "/mise/shims",
                "/usr/bin",
                "/bin",
            ]
        );
        assert_eq!(
            plan.env_pairs(),
            vec![
                ("FOO".into(), "bundle".into()),
                ("ZED".into(), "bundle-z".into()),
                ("BAR".into(), "nix".into()),
                ("BAZ".into(), "mise".into()),
            ]
        );
    }

    #[test]
    fn provider_credentials_are_filtered_with_the_bundle_filter() {
        let answer = ProviderAnswer::Ready(ActivationLayer::ready(
            "provider",
            Vec::new(),
            vec![
                ("AWS_SECRET_KEY".into(), "nope".into()),
                ("GH_TOKEN".into(), "nope".into()),
                ("EDITOR".into(), "hx".into()),
            ],
        ));
        let plan = compose_activation(&ResolvedEnv::default(), None, &[answer], None);
        assert_eq!(plan.env_pairs(), vec![("EDITOR".into(), "hx".into())]);
    }
}
