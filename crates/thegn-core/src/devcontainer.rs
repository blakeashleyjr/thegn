//! devcontainer.json parsing (the [Dev Container spec](https://containers.dev)).
//!
//! Pure, substrate-agnostic, unit-tested — the same shape as [`crate::envplan`]:
//! this module *reads and normalizes* a repo's `devcontainer.json` into a
//! [`DevContainer`] value; it never launches anything. Higher layers
//! (`devcontainer_overlay`) fold the result onto a `SandboxConfig` and emit
//! provisioning steps, and the repo-trust gate decides whether the (repo-
//! committed, arbitrary-code-bearing) declaration may be applied at all.
//!
//! devcontainer.json is JSONC (`//`/`/* */` comments + trailing commas). We
//! strip those to strict JSON in [`strip_jsonc`] and hand the result to
//! `serde_json` — no new dependency. The spec's polymorphic fields (lifecycle
//! commands that are `string | [argv] | {name: cmd}`, `forwardPorts` that are
//! `int | "h:c"`, `mounts` that are shorthand-string | object) are normalized
//! here in [`parse`] so downstream code sees one canonical shape.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A parsed, normalized `devcontainer.json`. Only the fields thegn can act
/// on are retained; `customizations.vscode.*` and other editor-only keys are
/// intentionally dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevContainer {
    /// Where the container image comes from: a pulled image, a Dockerfile
    /// build, or a compose service.
    pub source: ImageSource,
    /// `features` — OCI-packaged install units, kept raw (`id → options`).
    /// Resolution lives in [`crate::devcontainer_features`]; the parser only
    /// surfaces them so callers can order/gate on their presence.
    pub features: BTreeMap<String, Value>,
    /// `overrideFeatureInstallOrder` — explicit feature install order (ids,
    /// possibly without a tag/registry). Features not listed install after, in
    /// declaration order.
    pub override_feature_install_order: Vec<String>,
    /// Lifecycle commands, normalized per hook.
    pub lifecycle: Lifecycle,
    /// `mounts` — normalized bind/volume/tmpfs mounts.
    pub mounts: Vec<Mount>,
    /// `forwardPorts` — normalized to `"host:container"`.
    pub forward_ports: Vec<String>,
    /// `remoteEnv` — env applied to processes (not the container itself).
    pub remote_env: BTreeMap<String, String>,
    /// `containerEnv` — env baked into the container environment.
    pub container_env: BTreeMap<String, String>,
    /// `remoteUser` — user thegn's shell/exec runs as.
    pub remote_user: Option<String>,
    /// `containerUser` — user the container's entrypoint runs as.
    pub container_user: Option<String>,
    /// `workspaceFolder` — path the repo is mounted at inside the container.
    pub workspace_folder: Option<String>,
    /// `workspaceMount` — an explicit override of the workspace bind mount.
    pub workspace_mount: Option<String>,
    /// `runArgs` — raw args passed to the container runtime at create.
    pub run_args: Vec<String>,
    /// `overrideCommand` — whether to override the image's default command.
    pub override_command: Option<bool>,
    /// Absolute path to the directory containing this devcontainer.json (set by
    /// [`detect_and_parse`]; `None` when parsed from bare text). Build
    /// `context`/`dockerfile` and compose-file paths resolve against it.
    pub config_dir: Option<PathBuf>,
    /// Absolute path to the selected config file when loaded from a worktree.
    pub(crate) config_path: Option<PathBuf>,
    /// Top-level keys retained for the shared field-disposition inventory.
    pub(crate) source_keys: std::collections::BTreeSet<String>,
}

/// Where a dev container's image is sourced from.
#[derive(Debug, Clone, PartialEq)]
pub enum ImageSource {
    /// `image` — a pullable OCI reference.
    Image(String),
    /// `build` — a Dockerfile build.
    Build(Build),
    /// `dockerComposeFile` + `service` — a compose-managed service.
    Compose(Compose),
}

impl Default for ImageSource {
    fn default() -> Self {
        ImageSource::Image(String::new())
    }
}

/// A `build` block (`dockerfile`/`context`/`args`/`target`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Build {
    /// Path to the Dockerfile, relative to `.devcontainer/`.
    pub dockerfile: String,
    /// Build context directory (defaults to the devcontainer folder).
    pub context: String,
    /// `--build-arg` values.
    pub args: BTreeMap<String, String>,
    /// `--target` stage.
    pub target: Option<String>,
}

/// A `dockerComposeFile` block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compose {
    /// One or more compose files (spec allows a string or an array).
    pub files: Vec<String>,
    /// The service the dev container attaches to.
    pub service: String,
    /// Extra services to start alongside `service`.
    pub run_services: Vec<String>,
}

/// A normalized mount request.
#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    /// Host source (bind) or volume name (volume); `None` for anonymous.
    pub source: Option<String>,
    /// In-container destination path.
    pub target: String,
    /// Mount flavor.
    pub kind: MountKind,
    /// Whether the mount is read-only.
    pub readonly: bool,
}

/// Kind of a [`Mount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    /// Host-path bind mount.
    Bind,
    /// Named OCI volume.
    Volume,
    /// tmpfs.
    Tmpfs,
}

/// Lifecycle commands, grouped by when they run.
///
/// Each hook is a list because the spec's object form (`{name: cmd}`) declares
/// several commands that run in parallel; we flatten to a list (order within a
/// hook is not significant for the object form).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lifecycle {
    /// `initializeCommand` — runs on the **host**, before the container exists.
    pub initialize: Vec<Command>,
    /// `onCreateCommand` — one-time, at container creation.
    pub on_create: Vec<Command>,
    /// `updateContentCommand` — one-time, after content is available.
    pub update_content: Vec<Command>,
    /// `postCreateCommand` — one-time, after create.
    pub post_create: Vec<Command>,
    /// `postStartCommand` — every container start.
    pub post_start: Vec<Command>,
    /// `postAttachCommand` — every attach.
    pub post_attach: Vec<Command>,
}

impl Lifecycle {
    /// True when no lifecycle command is declared.
    pub fn is_empty(&self) -> bool {
        self.initialize.is_empty()
            && self.on_create.is_empty()
            && self.update_content.is_empty()
            && self.post_create.is_empty()
            && self.post_start.is_empty()
            && self.post_attach.is_empty()
    }
}

/// A single lifecycle command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A string command, run through a shell (`sh -c`).
    Shell(String),
    /// An argv array, executed directly without a shell.
    Argv(Vec<String>),
}

/// Why parsing a devcontainer.json failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The JSON (after comment stripping) was syntactically invalid.
    Json(String),
    /// The top-level value was not a JSON object.
    NotAnObject,
    /// The selected file could not be read.
    Read { path: PathBuf, error: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Json(e) => write!(f, "invalid devcontainer.json: {e}"),
            ParseError::NotAnObject => write!(f, "devcontainer.json is not a JSON object"),
            ParseError::Read { path, error } => {
                write!(f, "{}: cannot read: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Discover the devcontainer.json for a worktree, in the spec's precedence
/// order: `.devcontainer/devcontainer.json`, then `.devcontainer.json`, then
/// the first `.devcontainer/<name>/devcontainer.json` (sorted). Pure over the
/// filesystem; returns the path without reading it.
pub fn detect(worktree: &Path) -> Option<PathBuf> {
    crate::devcontainer_select::select_and_parse(worktree, None).selected
}

/// Detect + read + parse in one step. Returns `None` when no devcontainer.json
/// exists; propagates parse errors otherwise.
pub fn detect_and_parse(worktree: &Path) -> Option<Result<DevContainer, ParseError>> {
    let selected = crate::devcontainer_select::select_and_parse(worktree, None);
    if selected.candidates.is_empty() {
        return None;
    }
    Some(match selected.error {
        Some(crate::devcontainer_select::SelectionError::Ambiguous(paths)) => {
            Err(ParseError::Json(format!(
                "ambiguous devcontainer configs: {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
        Some(crate::devcontainer_select::SelectionError::Read { path, error }) => {
            Err(ParseError::Read { path, error })
        }
        Some(crate::devcontainer_select::SelectionError::Parse { error, .. }) => Err(error),
        Some(crate::devcontainer_select::SelectionError::SelectorNotFound { selector, .. }) => {
            Err(ParseError::Json(format!(
                "devcontainer selector {selector:?} matched no config"
            )))
        }
        None => Ok(selected.config.expect("successful selection has a config")),
    })
}

/// Parse devcontainer.json (JSONC) text into a normalized [`DevContainer`].
pub fn parse(text: &str) -> Result<DevContainer, ParseError> {
    let stripped = strip_jsonc(text);
    let value: Value =
        serde_json::from_str(&stripped).map_err(|e| ParseError::Json(e.to_string()))?;
    let obj = value.as_object().ok_or(ParseError::NotAnObject)?;

    Ok(DevContainer {
        source: parse_source(obj),
        features: obj
            .get("features")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        override_feature_install_order: obj
            .get("overrideFeatureInstallOrder")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        lifecycle: Lifecycle {
            initialize: cmd_field(obj, "initializeCommand"),
            on_create: cmd_field(obj, "onCreateCommand"),
            update_content: cmd_field(obj, "updateContentCommand"),
            post_create: cmd_field(obj, "postCreateCommand"),
            post_start: cmd_field(obj, "postStartCommand"),
            post_attach: cmd_field(obj, "postAttachCommand"),
        },
        mounts: obj
            .get("mounts")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(parse_mount).collect())
            .unwrap_or_default(),
        forward_ports: obj
            .get("forwardPorts")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(normalize_port).collect())
            .unwrap_or_default(),
        remote_env: str_map(obj, "remoteEnv"),
        container_env: str_map(obj, "containerEnv"),
        remote_user: str_field(obj, "remoteUser"),
        container_user: str_field(obj, "containerUser"),
        workspace_folder: str_field(obj, "workspaceFolder"),
        workspace_mount: str_field(obj, "workspaceMount"),
        run_args: obj
            .get("runArgs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        override_command: obj.get("overrideCommand").and_then(Value::as_bool),
        config_dir: None,
        config_path: None,
        source_keys: obj.keys().cloned().collect(),
    })
}

/// Stable identifier for a repo/config pair, suitable for `${devcontainerId}`
/// and deterministic volume names. The separator prevents path concatenation
/// collisions and no file contents are included, so edits keep the same id.
pub fn devcontainer_id(repo_root: &Path, config_relative_path: &Path) -> String {
    let key = format!(
        "{}\0{}",
        repo_root.to_string_lossy(),
        config_relative_path.to_string_lossy()
    );
    crate::util::short_hash(&key, 12)
}

/// Classify every source key using the shared inventory table.
pub fn recognized_unapplied(dc: &DevContainer) -> crate::devcontainer_inventory::FieldInventory {
    crate::devcontainer_inventory::classify_keys(&dc.source_keys)
}

/// Substitute the devcontainer variable syntax (`${...}`) in a string against a
/// [`SubstCtx`]. Unknown variables are left verbatim (the spec is lenient).
/// Pure — the environment lookups are supplied by the caller so this stays
/// testable.
pub fn substitute(input: &str, ctx: &SubstCtx) -> String {
    substitute_with_report(input, ctx, &|_| true).0
}

/// Names of local environment variables that were denied during expansion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubstitutionReport {
    pub blocked_local_env: std::collections::BTreeSet<String>,
}

/// Expand variables while enforcing a caller-supplied local environment
/// allowlist. Blocked values become empty and only their names are reported.
pub fn substitute_with_report(
    input: &str,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
) -> (String, SubstitutionReport) {
    substitute_with_id_report(input, ctx, None, local_env_allowed)
}

/// Expand variables with `${devcontainerId}` supplied by the selected config.
pub fn substitute_with_devcontainer_id(
    input: &str,
    ctx: &SubstCtx,
    devcontainer_id: &str,
) -> String {
    substitute_with_devcontainer_id_report(input, ctx, devcontainer_id, &|_| true).0
}

/// Expand variables with both the selected config id and local-env policy.
pub fn substitute_with_devcontainer_id_report(
    input: &str,
    ctx: &SubstCtx,
    devcontainer_id: &str,
    local_env_allowed: &dyn Fn(&str) -> bool,
) -> (String, SubstitutionReport) {
    substitute_with_id_report(input, ctx, Some(devcontainer_id), local_env_allowed)
}

fn substitute_with_id_report(
    input: &str,
    ctx: &SubstCtx,
    devcontainer_id: Option<&str>,
    local_env_allowed: &dyn Fn(&str) -> bool,
) -> (String, SubstitutionReport) {
    let mut out = String::with_capacity(input.len());
    let mut report = SubstitutionReport::default();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some(end) = input[i + 2..].find('}')
        {
            let name = &input[i + 2..i + 2 + end];
            out.push_str(&ctx.resolve(name, devcontainer_id, local_env_allowed, &mut report));
            i = i + 2 + end + 1;
            continue;
        }
        // Not a well-formed `${...}` — copy the char through on a UTF-8
        // boundary (never re-encode a multibyte code point as Latin-1).
        let ch = input[i..].chars().next().expect("valid utf-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, report)
}

/// Context for [`substitute`]: the values behind the devcontainer `${...}`
/// variables. `local_env`/`container_env` are closures so a caller can back
/// them with the real process environment without this module touching it.
pub struct SubstCtx<'a> {
    /// `${localWorkspaceFolder}` — host path of the workspace.
    pub local_workspace_folder: String,
    /// `${containerWorkspaceFolder}` — in-container path of the workspace.
    pub container_workspace_folder: String,
    /// `${localEnv:VAR}` lookup.
    pub local_env: &'a dyn Fn(&str) -> Option<String>,
    /// `${containerEnv:VAR}` lookup.
    pub container_env: &'a dyn Fn(&str) -> Option<String>,
}

impl SubstCtx<'_> {
    fn resolve(
        &self,
        name: &str,
        devcontainer_id: Option<&str>,
        local_env_allowed: &dyn Fn(&str) -> bool,
        report: &mut SubstitutionReport,
    ) -> String {
        let basename = |p: &str| {
            p.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(p)
                .to_string()
        };
        match name {
            "localWorkspaceFolder" => self.local_workspace_folder.clone(),
            "containerWorkspaceFolder" => self.container_workspace_folder.clone(),
            "localWorkspaceFolderBasename" => basename(&self.local_workspace_folder),
            "containerWorkspaceFolderBasename" => basename(&self.container_workspace_folder),
            "devcontainerId" => devcontainer_id
                .map(str::to_string)
                .unwrap_or_else(|| "${devcontainerId}".into()),
            _ => {
                if let Some(var) = name.strip_prefix("localEnv:") {
                    if local_env_allowed(var) {
                        (self.local_env)(var).unwrap_or_default()
                    } else {
                        report.blocked_local_env.insert(var.to_string());
                        String::new()
                    }
                } else if let Some(var) = name.strip_prefix("containerEnv:") {
                    (self.container_env)(var).unwrap_or_default()
                } else {
                    // Unknown variable: preserve it literally.
                    format!("${{{name}}}")
                }
            }
        }
    }
}

// ---- internal helpers -----------------------------------------------------

fn parse_source(obj: &serde_json::Map<String, Value>) -> ImageSource {
    // Precedence mirrors the reference implementation: compose wins over build
    // wins over image (a devcontainer with a compose file ignores `image`).
    if let Some(files) = compose_files(obj) {
        return ImageSource::Compose(Compose {
            files,
            service: str_field(obj, "service").unwrap_or_default(),
            run_services: obj
                .get("runServices")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    if let Some(build) = obj.get("build").and_then(Value::as_object) {
        // `dockerfile` (spec) with a legacy `dockerFile` alias.
        let dockerfile = build
            .get("dockerfile")
            .or_else(|| build.get("dockerFile"))
            .and_then(Value::as_str)
            .unwrap_or("Dockerfile")
            .to_string();
        return ImageSource::Build(Build {
            dockerfile,
            context: build
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string(),
            args: build
                .get("args")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| value_to_string(v).map(|s| (k.clone(), s)))
                        .collect()
                })
                .unwrap_or_default(),
            target: build
                .get("target")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    // Top-level `dockerFile`/`context` (older spec form) also implies a build.
    if let Some(dockerfile) = obj
        .get("dockerFile")
        .or_else(|| obj.get("dockerfile"))
        .and_then(Value::as_str)
    {
        return ImageSource::Build(Build {
            dockerfile: dockerfile.to_string(),
            context: str_field(obj, "context").unwrap_or_else(|| ".".to_string()),
            args: BTreeMap::new(),
            target: None,
        });
    }
    ImageSource::Image(str_field(obj, "image").unwrap_or_default())
}

fn compose_files(obj: &serde_json::Map<String, Value>) -> Option<Vec<String>> {
    match obj.get("dockerComposeFile")? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => {
            let v: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
            (!v.is_empty()).then_some(v)
        }
        _ => None,
    }
}

fn cmd_field(obj: &serde_json::Map<String, Value>, key: &str) -> Vec<Command> {
    obj.get(key).map(parse_command).unwrap_or_default()
}

/// Normalize a lifecycle-command value: `string | [argv] | {name: cmd}`.
fn parse_command(v: &Value) -> Vec<Command> {
    match v {
        Value::String(s) => vec![Command::Shell(s.clone())],
        Value::Array(a) => {
            let argv: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
            if argv.is_empty() {
                vec![]
            } else {
                vec![Command::Argv(argv)]
            }
        }
        // Object form: named commands that the spec runs in parallel. Values
        // are themselves `string | [argv]`.
        Value::Object(m) => m.values().flat_map(parse_command).collect(),
        _ => vec![],
    }
}

/// Normalize a `forwardPorts` entry (`int | "port" | "host:container"`) to
/// `"host:container"`.
fn normalize_port(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => n.as_u64().map(|p| format!("{p}:{p}")),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else if s.contains(':') {
                Some(s.to_string())
            } else {
                Some(format!("{s}:{s}"))
            }
        }
        _ => None,
    }
}

/// Normalize a `mounts` entry: object form or the compose/docker shorthand
/// string (`source=…,target=…,type=bind[,readonly]`).
fn parse_mount(v: &Value) -> Option<Mount> {
    match v {
        Value::Object(m) => {
            let target = m
                .get("target")
                .or_else(|| m.get("destination"))
                .or_else(|| m.get("dst"))
                .and_then(Value::as_str)?
                .to_string();
            let source = m
                .get("source")
                .or_else(|| m.get("src"))
                .and_then(Value::as_str)
                .map(String::from);
            let kind = m
                .get("type")
                .and_then(Value::as_str)
                .map(mount_kind)
                .unwrap_or(MountKind::Bind);
            let readonly = m.get("readonly").and_then(Value::as_bool).unwrap_or(false)
                || m.get("readOnly").and_then(Value::as_bool).unwrap_or(false);
            Some(Mount {
                source,
                target,
                kind,
                readonly,
            })
        }
        Value::String(s) => parse_mount_shorthand(s),
        _ => None,
    }
}

fn parse_mount_shorthand(s: &str) -> Option<Mount> {
    let mut source = None;
    let mut target = None;
    let mut kind = MountKind::Bind;
    let mut readonly = false;
    for part in s.split(',') {
        let (k, val) = part.split_once('=').unwrap_or((part.trim(), ""));
        match k.trim() {
            "source" | "src" => source = Some(val.trim().to_string()),
            "target" | "destination" | "dst" => target = Some(val.trim().to_string()),
            "type" => kind = mount_kind(val.trim()),
            "readonly" | "ro" => readonly = true,
            _ => {}
        }
    }
    Some(Mount {
        source: source.filter(|s| !s.is_empty()),
        target: target?,
        kind,
        readonly,
    })
}

fn mount_kind(s: &str) -> MountKind {
    match s {
        "volume" => MountKind::Volume,
        "tmpfs" => MountKind::Tmpfs,
        _ => MountKind::Bind,
    }
}

fn str_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(String::from)
}

fn str_map(obj: &serde_json::Map<String, Value>, key: &str) -> BTreeMap<String, String> {
    obj.get(key)
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| value_to_string(v).map(|s| (k.clone(), s)))
                .collect()
        })
        .unwrap_or_default()
}

/// Coerce a scalar JSON value to a string (env/build-arg values are sometimes
/// written as bare numbers or bools).
fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Strip JSONC (`//` line comments, `/* */` block comments, and trailing commas
/// before `}`/`]`) to strict JSON, preserving comment-like sequences inside
/// string literals.
pub fn strip_jsonc(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_str = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c < 0x80 {
                // ASCII: the only bytes that affect string state (`\`, `"`).
                out.push(c as char);
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
                i += 1;
            } else {
                // Multibyte UTF-8: copy the whole code point on its boundary so
                // non-ASCII string content isn't mangled into Latin-1 mojibake.
                let ch = input[i..].chars().next().expect("valid utf-8 boundary");
                out.push(ch);
                escaped = false;
                i += ch.len_utf8();
            }
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                out.push('"');
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment — skip to end of line.
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment — skip to `*/`.
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b',' => {
                // Drop a trailing comma: look ahead past whitespace/comments for
                // a closing bracket.
                if let Some(next) = next_significant(bytes, i + 1)
                    && (bytes[next] == b'}' || bytes[next] == b']')
                {
                    i += 1; // skip the comma
                    continue;
                }
                out.push(',');
                i += 1;
            }
            _ => {
                // Copy on a char boundary — a multibyte code point outside a
                // string (rare, but legal in JSON) must not be re-encoded.
                let ch = input[i..].chars().next().expect("valid utf-8 boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Index of the next non-whitespace, non-comment byte at or after `start`.
fn next_significant(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            _ => return Some(i),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dc(text: &str) -> DevContainer {
        parse(text).expect("parse")
    }

    #[test]
    fn image_only() {
        let d = dc(r#"{ "image": "debian:stable" }"#);
        assert_eq!(d.source, ImageSource::Image("debian:stable".into()));
        assert!(d.lifecycle.is_empty());
        assert!(d.features.is_empty());
    }

    #[test]
    fn strips_line_and_block_comments_and_trailing_commas() {
        let text = r#"{
            // a line comment
            "image": "ubuntu:22.04", /* inline */
            "forwardPorts": [3000, 8080,],
        }"#;
        let d = dc(text);
        assert_eq!(d.source, ImageSource::Image("ubuntu:22.04".into()));
        assert_eq!(d.forward_ports, vec!["3000:3000", "8080:8080"]);
    }

    #[test]
    fn comment_sequences_inside_strings_are_preserved() {
        let d = dc(r#"{ "image": "reg.io/x//y:1", "workspaceFolder": "/a/*b*/c" }"#);
        assert_eq!(d.source, ImageSource::Image("reg.io/x//y:1".into()));
        assert_eq!(d.workspace_folder.as_deref(), Some("/a/*b*/c"));
    }

    #[test]
    fn forward_ports_int_and_string_forms() {
        let d = dc(r#"{ "forwardPorts": [8000, "9000", "127.0.0.1:5432:5432"] }"#);
        assert_eq!(
            d.forward_ports,
            vec!["8000:8000", "9000:9000", "127.0.0.1:5432:5432"]
        );
    }

    #[test]
    fn lifecycle_string_array_and_object_forms() {
        let text = r#"{
            "image": "x",
            "initializeCommand": "echo host",
            "postCreateCommand": ["npm", "ci"],
            "postStartCommand": { "a": "svc-a", "b": ["svc", "b"] }
        }"#;
        let d = dc(text);
        assert_eq!(
            d.lifecycle.initialize,
            vec![Command::Shell("echo host".into())]
        );
        assert_eq!(
            d.lifecycle.post_create,
            vec![Command::Argv(vec!["npm".into(), "ci".into()])]
        );
        // Object form flattens (order is BTreeMap-stable: a, b).
        assert_eq!(
            d.lifecycle.post_start,
            vec![
                Command::Shell("svc-a".into()),
                Command::Argv(vec!["svc".into(), "b".into()]),
            ]
        );
    }

    #[test]
    fn build_block() {
        let text = r#"{
            "build": {
                "dockerfile": "Dockerfile.dev",
                "context": "..",
                "args": { "VARIANT": "20", "N": 3 },
                "target": "dev"
            }
        }"#;
        let d = dc(text);
        let ImageSource::Build(b) = d.source else {
            panic!("expected build");
        };
        assert_eq!(b.dockerfile, "Dockerfile.dev");
        assert_eq!(b.context, "..");
        assert_eq!(b.args.get("VARIANT").map(String::as_str), Some("20"));
        assert_eq!(b.args.get("N").map(String::as_str), Some("3"));
        assert_eq!(b.target.as_deref(), Some("dev"));
    }

    #[test]
    fn legacy_dockerfile_alias_and_toplevel_form() {
        let d = dc(r#"{ "build": { "dockerFile": "Containerfile" } }"#);
        assert!(matches!(d.source, ImageSource::Build(b) if b.dockerfile == "Containerfile"));
        let d2 = dc(r#"{ "dockerFile": "Dockerfile", "context": "app" }"#);
        assert!(matches!(d2.source, ImageSource::Build(b) if b.context == "app"));
    }

    #[test]
    fn compose_string_and_array_and_wins_over_image() {
        let d = dc(
            r#"{ "image": "ignored", "dockerComposeFile": "docker-compose.yml", "service": "app", "runServices": ["db"] }"#,
        );
        let ImageSource::Compose(c) = d.source else {
            panic!("expected compose");
        };
        assert_eq!(c.files, vec!["docker-compose.yml"]);
        assert_eq!(c.service, "app");
        assert_eq!(c.run_services, vec!["db"]);

        let d2 = dc(r#"{ "dockerComposeFile": ["a.yml", "b.yml"], "service": "web" }"#);
        assert!(matches!(d2.source, ImageSource::Compose(c) if c.files.len() == 2));
    }

    #[test]
    fn mounts_object_and_shorthand() {
        let text = r#"{
            "image": "x",
            "mounts": [
                { "source": "/host", "target": "/ctr", "type": "bind", "readonly": true },
                "source=vol,target=/data,type=volume",
                { "target": "/tmpx", "type": "tmpfs" }
            ]
        }"#;
        let d = dc(text);
        assert_eq!(
            d.mounts[0],
            Mount {
                source: Some("/host".into()),
                target: "/ctr".into(),
                kind: MountKind::Bind,
                readonly: true,
            }
        );
        assert_eq!(
            d.mounts[1],
            Mount {
                source: Some("vol".into()),
                target: "/data".into(),
                kind: MountKind::Volume,
                readonly: false,
            }
        );
        assert_eq!(d.mounts[2].kind, MountKind::Tmpfs);
        assert!(d.mounts[2].source.is_none());
    }

    #[test]
    fn env_users_runargs_and_features() {
        let text = r#"{
            "image": "x",
            "remoteEnv": { "FOO": "bar", "PORT": 8080 },
            "containerEnv": { "TZ": "UTC" },
            "remoteUser": "vscode",
            "containerUser": "root",
            "runArgs": ["--cap-add=SYS_PTRACE", "--security-opt", "seccomp=unconfined"],
            "overrideCommand": false,
            "features": { "ghcr.io/devcontainers/features/node:1": { "version": "20" } }
        }"#;
        let d = dc(text);
        assert_eq!(d.remote_env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(d.remote_env.get("PORT").map(String::as_str), Some("8080"));
        assert_eq!(d.container_env.get("TZ").map(String::as_str), Some("UTC"));
        assert_eq!(d.remote_user.as_deref(), Some("vscode"));
        assert_eq!(d.container_user.as_deref(), Some("root"));
        assert_eq!(d.run_args.len(), 3);
        assert_eq!(d.override_command, Some(false));
        assert!(
            d.features
                .contains_key("ghcr.io/devcontainers/features/node:1")
        );
    }

    #[test]
    fn invalid_json_errs() {
        assert!(matches!(parse("{ not json"), Err(ParseError::Json(_))));
        assert!(matches!(parse("[1,2,3]"), Err(ParseError::NotAnObject)));
    }

    #[test]
    fn selected_read_and_parse_failures_are_surfaced() {
        let root = std::env::temp_dir().join(format!("tg-dc-errors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
        std::fs::write(root.join(".devcontainer/devcontainer.json"), "{ broken").unwrap();
        assert!(matches!(
            detect_and_parse(&root),
            Some(Err(ParseError::Json(_)))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn substitute_variables() {
        let ctx = SubstCtx {
            local_workspace_folder: "/home/u/proj".into(),
            container_workspace_folder: "/workspaces/proj".into(),
            local_env: &|v| (v == "USER").then(|| "alice".to_string()),
            container_env: &|_| None,
        };
        assert_eq!(
            substitute("${localWorkspaceFolder}/x", &ctx),
            "/home/u/proj/x"
        );
        assert_eq!(substitute("${localWorkspaceFolderBasename}", &ctx), "proj");
        assert_eq!(
            substitute("${containerWorkspaceFolder}", &ctx),
            "/workspaces/proj"
        );
        assert_eq!(substitute("u=${localEnv:USER}", &ctx), "u=alice");
        assert_eq!(substitute("m=${localEnv:MISSING}", &ctx), "m=");
        // Unknown variables are preserved literally.
        assert_eq!(substitute("${weird}", &ctx), "${weird}");
    }

    #[test]
    fn substitution_reports_blocked_local_env_and_supports_stable_id() {
        let ctx = SubstCtx {
            local_workspace_folder: "/work/repo".into(),
            container_workspace_folder: "/workspaces/repo".into(),
            local_env: &|name| (name == "SAFE").then(|| "ok".into()),
            container_env: &|_| None,
        };
        let (value, report) = substitute_with_devcontainer_id_report(
            "${localEnv:SAFE}/${localEnv:SECRET}/${devcontainerId}",
            &ctx,
            "abc123",
            &|name| name == "SAFE",
        );
        assert_eq!(value, "ok//abc123");
        assert_eq!(
            report.blocked_local_env.iter().collect::<Vec<_>>(),
            [&"SECRET".to_string()]
        );
        assert_eq!(
            devcontainer_id(
                Path::new("/repo"),
                Path::new(".devcontainer/a/devcontainer.json")
            ),
            devcontainer_id(
                Path::new("/repo"),
                Path::new(".devcontainer/a/devcontainer.json")
            )
        );
        assert_ne!(
            devcontainer_id(
                Path::new("/repo"),
                Path::new(".devcontainer/a/devcontainer.json")
            ),
            devcontainer_id(
                Path::new("/repo"),
                Path::new(".devcontainer/b/devcontainer.json")
            )
        );
    }

    #[test]
    fn non_ascii_strings_round_trip_without_mojibake() {
        // Non-ASCII path/env values must survive strip_jsonc + parse intact —
        // no Latin-1 doubling ("José" must not become "JosÃ©").
        let text = r#"{
            "image": "x",
            // café ☕ — comment with unicode is dropped
            "workspaceFolder": "/home/josé/proj",
            "containerEnv": { "GREETING": "héllo wörld ☕" }
        }"#;
        let d = dc(text);
        assert_eq!(d.workspace_folder.as_deref(), Some("/home/josé/proj"));
        assert_eq!(
            d.container_env.get("GREETING").map(String::as_str),
            Some("héllo wörld ☕")
        );
    }

    #[test]
    fn substitute_preserves_non_ascii() {
        let ctx = SubstCtx {
            local_workspace_folder: "/home/josé".into(),
            container_workspace_folder: String::new(),
            local_env: &|_| None,
            container_env: &|_| None,
        };
        // Multibyte content on the literal (non-`${}`) path is copied verbatim.
        assert_eq!(
            substitute("café ☕ ${localWorkspaceFolder}", &ctx),
            "café ☕ /home/josé"
        );
    }

    #[test]
    fn detect_precedence(/* fs */) {
        let tmp = std::env::temp_dir().join(format!("tg-dc-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp); // best-effort: test cleanup: scratch removal must never fail the test
        std::fs::create_dir_all(tmp.join(".devcontainer/sub")).unwrap();
        // Only the sub-config exists first.
        std::fs::write(tmp.join(".devcontainer/sub/devcontainer.json"), "{}").unwrap();
        assert_eq!(
            detect(&tmp).unwrap().file_name().unwrap(),
            "devcontainer.json"
        );
        assert!(detect(&tmp).unwrap().to_string_lossy().contains("sub"));
        // Root dotfile outranks the sub-config.
        std::fs::write(tmp.join(".devcontainer.json"), "{}").unwrap();
        assert!(detect(&tmp).unwrap().ends_with(".devcontainer.json"));
        // Primary outranks everything.
        std::fs::write(tmp.join(".devcontainer/devcontainer.json"), "{}").unwrap();
        assert_eq!(
            detect(&tmp).unwrap(),
            tmp.join(".devcontainer/devcontainer.json")
        );
        let _ = std::fs::remove_dir_all(&tmp); // best-effort: test cleanup: scratch removal must never fail the test
    }
}
