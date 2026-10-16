//! Docker Compose sandboxes — split out of the (ratchet-capped) `sandbox.rs`.
//!
//! A devcontainer `dockerComposeFile` + `service` needs more than the single
//! file path the legacy `[sandbox] compose` string carried: the file list, the
//! service to attach a pane to, and any extra `runServices`. Rather than grow
//! `SandboxConfig` with new fields (the config god-file is at its ratchet
//! ceiling), we **pack** that structure into the existing `compose: Option<String>`
//! field with control-character separators that never appear in paths, and
//! decode it here. A plain path (no separators) still decodes as a single-file,
//! no-service compose — so pre-existing `[sandbox] compose = "docker-compose.yml"`
//! configs are unchanged.
//!
//! With a `service`, the pane enters the container via `docker compose exec
//! <service>` (no container-name guessing), and `ensure` brings the project up
//! with `up -d <service> <runServices…>`. The argv builders are pure + tested;
//! only the callers run subprocesses.

use crate::placement::Placement;
use crate::sandbox::{Backend, SandboxSpec, oci_prefix, oci_prefix_for};
use crate::sandbox_manage::{OWNED_LABEL, OWNER_LABEL_KEY, owner_identity};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Maximum time allowed for `docker compose up`, including any pull it triggers.
pub(crate) const COMPOSE_UP_TIMEOUT: Duration = Duration::from_secs(150);
/// Maximum time allowed to verify that Compose reports a running service.
pub(crate) const COMPOSE_PS_TIMEOUT: Duration = Duration::from_secs(5);
/// Effective Compose inspection is optional, but must not delay launch for long.
pub(crate) const COMPOSE_CONFIG_TIMEOUT: Duration = Duration::from_secs(15);
const COMPOSE_CONFIG_STDOUT_LIMIT: usize = 1024 * 1024;
const COMPOSE_CONFIG_STDERR_LIMIT: usize = 16 * 1024;
/// Compose's default stop grace exceeds the ordinary five-second control probe.
pub(crate) const COMPOSE_DOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Build `compose -p <project> down --remove-orphans` for supported engines.
pub(crate) fn down_argv_for(
    backend: Backend,
    oci_host: Option<&str>,
    project: &str,
) -> Option<Vec<String>> {
    if !matches!(
        backend,
        Backend::Docker | Backend::Podman | Backend::PodmanRootful
    ) {
        return None;
    }
    let mut argv = oci_prefix_for(backend, oci_host);
    argv.extend([
        "compose".into(),
        "-p".into(),
        project.into(),
        "down".into(),
        "--remove-orphans".into(),
    ]);
    Some(argv)
}

/// Compose projects use a path-specific suffix while ordinary OCI container
/// names retain their historic slug-only form.
pub(crate) fn hashed_project_name(name: &str, worktree: &Path) -> String {
    format!(
        "{name}-{}",
        crate::util::short_hash(&worktree.to_string_lossy(), 6)
    )
}

fn adoption_record_path(state_home: &Path, name: &str, worktree: &Path) -> PathBuf {
    state_home
        .join("thegn/compose-projects")
        .join(format!("{}.adopted", hashed_project_name(name, worktree)))
}

fn adoption_record_matches(path: &Path, legacy_name: &str) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::read_to_string(path).is_ok_and(|body| body.trim() == legacy_name)
        }
        _ => false,
    }
}

fn adoption_record_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

pub(crate) fn project_for_in(state_home: &Path, spec: &SandboxSpec) -> String {
    let path = adoption_record_path(state_home, &spec.name, &spec.worktree);
    if adoption_record_matches(&path, &spec.name) {
        spec.name.clone()
    } else {
        hashed_project_name(&spec.name, &spec.worktree)
    }
}

pub(crate) fn project_for(spec: &SandboxSpec) -> String {
    project_for_in(&crate::util::xdg_state_home(), spec)
}

/// True when a Compose working directory belongs to this worktree, using path
/// components so similarly slugified sibling paths cannot adopt one another.
pub(crate) fn legacy_owned_by(stdout: &str, worktree: &Path) -> bool {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .any(|line| {
            let path = Path::new(line);
            path.is_absolute() && path.starts_with(worktree)
        })
}

fn write_adoption_record(state_home: &Path, spec: &SandboxSpec) -> std::io::Result<()> {
    let directory =
        crate::fsperm::ensure_private_dir_beneath(state_home, Path::new("thegn/compose-projects"))?;
    directory.write_owner_only_atomic(
        &format!(
            "{}.adopted",
            hashed_project_name(&spec.name, &spec.worktree)
        ),
        spec.name.as_bytes(),
    )?;
    Ok(())
}

pub(crate) fn resolve_project_with(
    state_home: &Path,
    spec: &SandboxSpec,
    target: &ComposeTarget,
    runner: impl Fn(&[String]) -> Option<(bool, String)>,
) -> String {
    let hashed = hashed_project_name(&spec.name, &spec.worktree);
    let record = adoption_record_path(state_home, &spec.name, &spec.worktree);
    if adoption_record_exists(&record) {
        return if adoption_record_matches(&record, &spec.name) {
            spec.name.clone()
        } else {
            hashed
        };
    }

    let mut ps = crate::sandbox::oci_prefix(spec);
    ps.extend([
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("label=com.docker.compose.project={}", spec.name),
        "--format".into(),
        "{{.Names}}".into(),
    ]);
    let Some((true, names)) = runner(&target.control_argv(&ps)) else {
        tracing::info!(target: "thegn::sandbox", "unable to probe legacy Compose project; using hashed project name");
        return hashed;
    };
    let names = names
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if names.is_empty() {
        return hashed;
    }
    let mut inspect = crate::sandbox::oci_prefix(spec);
    inspect.extend([
        "inspect".into(),
        "--format".into(),
        "{{index .Config.Labels \"com.docker.compose.project.working_dir\"}}".into(),
    ]);
    inspect.extend(names.into_iter().map(str::to_owned));
    let Some((true, working_dirs)) = runner(&target.control_argv(&inspect)) else {
        tracing::info!(target: "thegn::sandbox", "unable to inspect legacy Compose project; using hashed project name");
        return hashed;
    };
    if legacy_owned_by(&working_dirs, &spec.worktree) {
        match write_adoption_record(state_home, spec) {
            Ok(()) => spec.name.clone(),
            Err(error) => {
                tracing::info!(target: "thegn::sandbox", %error, "unable to record adopted Compose project; using hashed project name");
                hashed
            }
        }
    } else {
        hashed
    }
}

fn probe_project(state_home: &Path, spec: &SandboxSpec, target: &ComposeTarget) -> String {
    use crate::sandbox::proc::{Capture, run_bounded};

    resolve_project_with(state_home, spec, target, |argv| {
        run_bounded(
            argv,
            COMPOSE_PS_TIMEOUT,
            Capture {
                stdout: Some(16 * 1024),
                stderr: Some(4096),
            },
        )
        .into_stdout()
    })
}

pub(crate) fn teardown_projects_in(state_home: &Path, worktree: &Path) -> Vec<String> {
    let name = crate::sandbox::container_name(&worktree.to_string_lossy());
    let hashed = hashed_project_name(&name, worktree);
    let record = adoption_record_path(state_home, &name, worktree);
    if adoption_record_matches(&record, &name) {
        vec![hashed, name]
    } else {
        vec![hashed]
    }
}

/// A bounded Compose startup or verification failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposeError {
    Unsupported { backend: &'static str },
    Spawn,
    Timeout,
    Exit { code: Option<i32>, stderr: String },
    OutputLimit,
    NotRunning,
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { backend } => {
                write!(f, "Compose is unsupported for backend '{backend}'")
            }
            Self::Spawn => write!(f, "unable to run Compose command"),
            Self::Timeout => write!(f, "Compose command timed out"),
            Self::Exit { code, stderr } => write!(
                f,
                "Compose exited with status {code:?}: {:?}",
                stderr.trim()
            ),
            Self::OutputLimit => write!(f, "Compose output exceeded its size limit"),
            Self::NotRunning => write!(f, "Compose started no running service"),
        }
    }
}

impl std::error::Error for ComposeError {}

/// Host source in the effective Compose model reaches a path protected by thegn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeBindRefusal {
    pub service: String,
    pub source: String,
    pub class: String,
}

impl fmt::Display for ComposeBindRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Compose service '{}' binds protected host path '{}' ({}); remove that bind. No per-repo opt-out exists.",
            self.service, self.source, self.class
        )
    }
}

impl std::error::Error for ComposeBindRefusal {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnsureReport {
    pub compose_unverified: Option<String>,
}

/// Report host-process isolation whenever Compose config could not be checked.
pub fn compose_reported_isolation(
    base: crate::capabilities::IsolationClass,
    report: &EnsureReport,
) -> crate::capabilities::IsolationClass {
    if report.compose_unverified.is_some() {
        crate::capabilities::IsolationClass::HostProcess
    } else {
        base
    }
}

#[derive(Debug, Clone, Default)]
pub struct ComposeGuard {
    pub protected: Vec<PathBuf>,
    classes: Vec<String>,
}

impl ComposeGuard {
    /// Local host paths Compose must not make reachable to a sandbox service.
    pub fn for_host(extra: &[PathBuf]) -> Self {
        let mut protected = Vec::new();
        let mut classes = Vec::new();
        {
            let mut add = |path: PathBuf, class: &str| {
                protected.push(path);
                classes.push(class.to_owned());
            };
            add(crate::util::xdg_state_home().join("thegn"), "thegn state");
            if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
                add(PathBuf::from(runtime).join("thegn"), "thegn runtime");
            }
            if let Some(home) = std::env::var_os("HOME") {
                let home = PathBuf::from(home);
                for path in [
                    ".ssh",
                    ".aws",
                    ".gnupg",
                    ".config/gh",
                    ".kube",
                    ".docker",
                    ".netrc",
                    ".git-credentials",
                ] {
                    add(home.join(path), "credential");
                }
                add(home.join(".docker/run/docker.sock"), "engine socket");
            }
            for path in [
                "/var/run/docker.sock",
                "/run/docker.sock",
                "/run/podman/podman.sock",
                "/run/containerd/containerd.sock",
            ] {
                add(PathBuf::from(path), "engine socket");
            }
            if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
                add(
                    PathBuf::from(runtime).join("podman/podman.sock"),
                    "engine socket",
                );
            }
            for path in extra {
                add(path.clone(), "control socket");
            }
        }
        Self { protected, classes }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigParseError(String);

impl fmt::Display for ConfigParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigParseError {}

/// Compose config --format json has resolved inheritance, interpolation and paths.
pub fn protected_binds(
    config_json: &str,
    protected: &[PathBuf],
) -> Result<Option<ComposeBindRefusal>, ConfigParseError> {
    protected_binds_with_classes(config_json, protected, &[])
}

fn protected_binds_with_classes(
    config_json: &str,
    protected: &[PathBuf],
    classes: &[String],
) -> Result<Option<ComposeBindRefusal>, ConfigParseError> {
    let root: serde_json::Value =
        serde_json::from_str(config_json).map_err(|error| ConfigParseError(error.to_string()))?;
    let services = root
        .get("services")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            ConfigParseError("effective Compose config has no services object".into())
        })?;
    let mut refs = Vec::<(String, String)>::new();
    let mut service_refs = Vec::<(String, String)>::new();
    for (service, definition) in services {
        let Some(volumes) = definition
            .get("volumes")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for volume in volumes {
            if let Some(volume) = volume.as_object() {
                let kind = volume
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if kind == "bind"
                    && let Some(source) = volume.get("source").and_then(serde_json::Value::as_str)
                {
                    service_refs.push((service.clone(), source.to_owned()));
                } else if kind == "volume"
                    && let Some(name) = volume.get("source").and_then(serde_json::Value::as_str)
                {
                    refs.push((service.clone(), format!("@volume:{name}")));
                }
            } else if let Some(short) = volume.as_str()
                && let Some(source) = short_mount_source(short)
            {
                service_refs.push((service.clone(), source));
            }
        }
    }
    let top_volumes = root.get("volumes").and_then(serde_json::Value::as_object);
    for (service, source) in refs {
        let Some(name) = source.strip_prefix("@volume:") else {
            continue;
        };
        if let Some(device) = top_volumes
            .and_then(|volumes| volumes.get(name))
            .and_then(|volume| volume.get("driver_opts"))
            .and_then(|opts| opts.get("device"))
            .and_then(serde_json::Value::as_str)
            && top_volumes
                .and_then(|volumes| volumes.get(name))
                .and_then(|volume| volume.get("driver_opts"))
                .and_then(|opts| opts.get("o"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|options| {
                    options
                        .split(',')
                        .map(str::trim)
                        .any(|option| matches!(option, "bind" | "rbind"))
                })
        {
            service_refs.push((service, device.to_owned()));
        }
    }
    for section in ["secrets", "configs"] {
        let Some(definitions) = root.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, definition) in definitions {
            let Some(source) = definition.get("file").and_then(serde_json::Value::as_str) else {
                continue;
            };
            for (service, service_def) in services {
                if service_def
                    .get(section)
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.as_str() == Some(name)
                                || item.get("source").and_then(serde_json::Value::as_str)
                                    == Some(name)
                        })
                    })
                {
                    service_refs.push((service.clone(), source.to_owned()));
                }
            }
        }
    }
    for (service, source) in service_refs {
        let socket_name = Path::new(&source)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| ["docker.sock", "podman.sock", "containerd.sock"].contains(&name));
        if socket_name {
            return Ok(Some(ComposeBindRefusal {
                service,
                source,
                class: "engine socket".into(),
            }));
        }
        if crate::sandbox_mounts::path_reaches_protected(Path::new(&source), protected) {
            let class = protected
                .iter()
                .position(|entry| {
                    crate::sandbox_mounts::path_reaches_protected(
                        Path::new(&source),
                        std::slice::from_ref(entry),
                    )
                })
                .and_then(|index| classes.get(index))
                .cloned()
                .unwrap_or_else(|| protected_path_class(protected));
            return Ok(Some(ComposeBindRefusal {
                service,
                source,
                class,
            }));
        }
    }
    Ok(None)
}

fn protected_path_class(protected: &[PathBuf]) -> String {
    for path in protected {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if ["docker.sock", "podman.sock", "containerd.sock"].contains(&name) {
            return "engine socket".into();
        }
        if [
            ".ssh",
            ".aws",
            ".gnupg",
            "gh",
            ".kube",
            ".docker",
            ".netrc",
            ".git-credentials",
        ]
        .contains(&name)
        {
            return "credential".into();
        }
        if path.ends_with("thegn") {
            if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR")
                && path == &PathBuf::from(runtime).join("thegn")
            {
                return "thegn runtime".into();
            }
            return "thegn state".into();
        }
    }
    "control socket".into()
}

fn short_mount_source(short: &str) -> Option<String> {
    let source = short.split(':').next()?;
    (source.starts_with('/')
        || source.starts_with("./")
        || source.starts_with("../")
        || source == "."
        || source == ".."
        || source.starts_with('~'))
    .then(|| source.to_owned())
}

/// Field separator (US) between the file-list, service, and run-services parts.
const FS: char = '\u{1f}';
/// List separator (RS) within the file-list / run-services parts.
const LS: char = '\u{1e}';

/// A decoded compose declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposeSpec {
    /// One or more compose files (each passed as `-f`).
    pub files: Vec<String>,
    /// The service a pane attaches to (`docker compose exec <service>`). `None`
    /// for the legacy single-file form (attach handled the old way).
    pub service: Option<String>,
    /// Extra services to start alongside `service`.
    pub run_services: Vec<String>,
}

impl ComposeSpec {
    /// Pack into the `compose` string field. A single file with no service
    /// encodes as the bare path (backward-compatible).
    pub fn encode(&self) -> String {
        if self.service.is_none() && self.run_services.is_empty() && self.files.len() <= 1 {
            return self.files.first().cloned().unwrap_or_default();
        }
        format!(
            "{}{FS}{}{FS}{}",
            self.files.join(&LS.to_string()),
            self.service.clone().unwrap_or_default(),
            self.run_services.join(&LS.to_string()),
        )
    }

    /// Decode from the `compose` string field. A string with no field separator
    /// is a legacy single-file path.
    pub fn decode(s: &str) -> ComposeSpec {
        if !s.contains(FS) {
            return ComposeSpec {
                files: vec![s.to_string()],
                service: None,
                run_services: Vec::new(),
            };
        }
        let mut parts = s.splitn(3, FS);
        let files = parts
            .next()
            .unwrap_or("")
            .split(LS)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        let service = parts.next().filter(|s| !s.is_empty()).map(String::from);
        let run_services = parts
            .next()
            .unwrap_or("")
            .split(LS)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        ComposeSpec {
            files,
            service,
            run_services,
        }
    }

    /// True when a pane should attach through `docker compose exec` (a service
    /// is named).
    pub fn has_service(&self) -> bool {
        self.service.is_some()
    }
}

/// Resolved Compose engine and transport. Exec argv stays unwrapped; pane entry
/// owns interactive wrapping and preflight owns control wrapping.
#[derive(Debug, Clone)]
pub(crate) struct ComposeTarget {
    engine: Vec<String>,
    placement: Placement,
}

impl ComposeTarget {
    pub(crate) fn for_spec(spec: &SandboxSpec) -> Result<Self, ComposeError> {
        if !matches!(
            spec.backend,
            Backend::Docker | Backend::Podman | Backend::PodmanRootful
        ) {
            return Err(ComposeError::Unsupported {
                backend: spec.backend.label(),
            });
        }
        let mut engine = oci_prefix(spec);
        engine.push("compose".into());
        Ok(Self {
            engine,
            placement: spec.placement.clone(),
        })
    }

    pub(crate) fn control_argv(&self, argv: &[String]) -> Vec<String> {
        self.placement.control_argv(argv)
    }
}

/// `<engine> compose -f … -p <project> …<rest>` prefix shared by up/exec.
fn project_prefix(target: &ComposeTarget, project: &str, c: &ComposeSpec) -> Vec<String> {
    let mut v = target.engine.clone();
    for f in &c.files {
        v.push("-f".into());
        v.push(f.clone());
    }
    v.push("-p".into());
    v.push(project.to_string());
    v
}

/// `docker compose … config --services`. Pure; used to enumerate services
/// that receive the owner-label override. Compose omits profile-gated services
/// unless profiles are enabled, so services started by profile remain
/// unlabelled and are kept by GC as unverified.
pub(crate) fn services_argv(target: &ComposeTarget, project: &str, c: &ComposeSpec) -> Vec<String> {
    let mut argv = project_prefix(target, project, c);
    argv.extend(["config".into(), "--services".into()]);
    argv
}

/// `docker compose … config --format json`. This uses the final file list,
/// including the owner-label override, and the same project and engine as up.
pub(crate) fn config_json_argv(
    target: &ComposeTarget,
    project: &str,
    c: &ComposeSpec,
) -> Vec<String> {
    let mut argv = project_prefix(target, project, c);
    argv.extend(["config".into(), "--format".into(), "json".into()]);
    argv.extend(c.service.iter().cloned());
    argv.extend(c.run_services.iter().cloned());
    argv
}

/// Render deterministic Compose labels for safe service identifiers only.
pub(crate) fn owner_label_override(services: &[String], owner: &str) -> Option<String> {
    let (managed_key, managed_value) = OWNED_LABEL.split_once('=')?;
    let mut labelled = serde_json::Map::new();
    for service in services.iter().filter(|service| {
        !service.is_empty()
            && service
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    }) {
        let mut labels = serde_json::Map::new();
        labels.insert(
            managed_key.into(),
            serde_json::Value::String(managed_value.into()),
        );
        labels.insert(
            OWNER_LABEL_KEY.into(),
            serde_json::Value::String(owner.into()),
        );
        labelled.insert(service.clone(), serde_json::json!({"labels": labels}));
    }
    (!labelled.is_empty()).then(|| serde_json::json!({"services": labelled}).to_string())
}

/// Add a private owner-label override when bounded service enumeration and
/// the private-file writer both succeed. Failure is fail-safe: Compose remains
/// usable, and GC keeps the resulting unverified containers.
fn labelled_compose(
    compose: &ComposeSpec,
    services_stdout: Option<&str>,
    owner: &str,
    existing: Option<PathBuf>,
    write: impl FnOnce(&str) -> std::io::Result<PathBuf>,
) -> ComposeSpec {
    let with_existing = || {
        if let Some(path) = &existing {
            let mut labelled = compose.clone();
            labelled.files.push(path.to_string_lossy().into_owned());
            labelled
        } else {
            compose.clone()
        }
    };
    let Some(services_stdout) = services_stdout else {
        return with_existing();
    };
    let services = services_stdout
        .lines()
        .map(str::trim)
        .filter(|service| !service.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let Some(body) = owner_label_override(&services, owner) else {
        return with_existing();
    };
    match write(&body) {
        Ok(path) => {
            let mut labelled = compose.clone();
            labelled.files.push(path.to_string_lossy().into_owned());
            labelled
        }
        Err(error) => {
            tracing::info!(target: "thegn::sandbox", %error, "unable to write Compose owner-label override; reusing existing override when available");
            with_existing()
        }
    }
}

fn enumerate_services(target: &ComposeTarget, argv: &[String]) -> Option<String> {
    use crate::sandbox::proc::{Capture, run_bounded};

    let outcome = run_bounded(
        &target.control_argv(argv),
        COMPOSE_PS_TIMEOUT,
        Capture {
            stdout: Some(16 * 1024),
            stderr: Some(4096),
        },
    );
    match outcome.into_stdout() {
        Some((true, stdout)) => Some(stdout),
        Some((false, _)) | None => {
            tracing::info!(target: "thegn::sandbox", "unable to enumerate Compose services for owner labels");
            None
        }
    }
}

pub(crate) fn sweep_stale_label_files_in(
    anchor: &std::path::Path,
    now: std::time::SystemTime,
    max_age: Duration,
) -> usize {
    let rel = std::path::Path::new("thegn/compose-labels");
    let dir = anchor.join(rel);
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return 0,
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "unable to inspect Compose label directory");
            return 0;
        }
    }
    match crate::fsperm::ensure_private_dir_beneath(anchor, rel) {
        Ok(directory) => directory.remove_stale_files("json", now, max_age),
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "unable to pin Compose label directory");
            0
        }
    }
}

/// `docker compose … up -d [service runServices…]`. Pure.
pub(crate) fn up_argv(target: &ComposeTarget, project: &str, c: &ComposeSpec) -> Vec<String> {
    let mut v = project_prefix(target, project, c);
    v.push("up".into());
    v.push("-d".into());
    if let Some(svc) = &c.service {
        v.push(svc.clone());
    }
    v.extend(c.run_services.iter().cloned());
    v
}

/// `docker compose … ps --status running --quiet [service]`. Pure.
pub(crate) fn ps_argv(target: &ComposeTarget, project: &str, c: &ComposeSpec) -> Vec<String> {
    let mut v = project_prefix(target, project, c);
    v.extend([
        "ps".into(),
        "--status".into(),
        "running".into(),
        "--quiet".into(),
    ]);
    if let Some(service) = &c.service {
        v.push(service.clone());
    }
    v
}

/// Run Compose startup and confirm at least one intended service is running.
pub(crate) fn run_up_checked(
    up: &[String],
    ps: &[String],
    up_timeout: Duration,
    ps_timeout: Duration,
) -> Result<(), ComposeError> {
    use crate::sandbox::proc::{Capture, ProcOutcome, run_bounded};

    let up_outcome = run_bounded(
        up,
        up_timeout,
        Capture {
            stdout: None,
            stderr: Some(64 * 1024),
        },
    );
    let (up_success, up_code) = match &up_outcome {
        ProcOutcome::Exited { status, .. } => (status.success(), status.code()),
        ProcOutcome::Timeout => return Err(ComposeError::Timeout),
        ProcOutcome::Spawn | ProcOutcome::Wait | ProcOutcome::Read => {
            return Err(ComposeError::Spawn);
        }
    };
    if !up_success {
        let stderr = up_outcome
            .into_stderr()
            .map(|(_, stderr)| stderr)
            .unwrap_or_else(|| "<stderr truncated>".into());
        return Err(ComposeError::Exit {
            code: up_code,
            stderr,
        });
    }
    if up_outcome.into_stderr().is_none() {
        return Err(ComposeError::OutputLimit);
    }

    let ps_outcome = run_bounded(
        ps,
        ps_timeout,
        Capture {
            stdout: Some(4096),
            stderr: Some(4096),
        },
    );
    let (ps_success, ps_code) = match &ps_outcome {
        ProcOutcome::Exited { status, .. } => (status.success(), status.code()),
        ProcOutcome::Timeout => return Err(ComposeError::Timeout),
        ProcOutcome::Spawn | ProcOutcome::Wait | ProcOutcome::Read => {
            return Err(ComposeError::Spawn);
        }
    };
    if !ps_success {
        let stderr = ps_outcome
            .into_stderr()
            .map(|(_, stderr)| stderr)
            .unwrap_or_else(|| "<stderr truncated>".into());
        return Err(ComposeError::Exit {
            code: ps_code,
            stderr,
        });
    }
    let (_, running) = ps_outcome.into_stdout().ok_or(ComposeError::OutputLimit)?;
    if running.trim().is_empty() {
        return Err(ComposeError::NotRunning);
    }
    Ok(())
}

/// Inspect effective config before starting it. Tool availability failures permit
/// launch with a weaker attestation report; malformed successful output refuses.
fn up_attested(
    config: &[String],
    up: &[String],
    ps: &[String],
    guard: &ComposeGuard,
    timeouts: (Duration, Duration, Duration),
) -> anyhow::Result<EnsureReport> {
    use crate::sandbox::proc::{Capture, ProcOutcome, run_bounded};

    let inspection = run_bounded(
        config,
        timeouts.0,
        Capture {
            stdout: Some(COMPOSE_CONFIG_STDOUT_LIMIT),
            stderr: Some(COMPOSE_CONFIG_STDERR_LIMIT),
        },
    );
    let status = match &inspection {
        ProcOutcome::Exited { status, .. } => Some((status.success(), status.code())),
        ProcOutcome::Timeout | ProcOutcome::Spawn | ProcOutcome::Wait | ProcOutcome::Read => None,
    };
    let verified = match status {
        Some((true, _)) => match inspection.into_stdout() {
            Some((true, output)) => {
                match protected_binds_with_classes(&output, &guard.protected, &guard.classes) {
                    Ok(Some(refusal)) => return Err(anyhow::Error::new(refusal)),
                    Ok(None) => None,
                    Err(error) => {
                        return Err(anyhow::anyhow!(
                            "effective Compose config could not be verified: {error}"
                        ));
                    }
                }
            }
            Some((false, _)) | None => {
                return Err(anyhow::anyhow!(
                    "effective Compose config could not be verified: output exceeded its size limit"
                ));
            }
        },
        Some((false, code)) => Some(format!("Compose config exited with status {code:?}")),
        None => match inspection {
            ProcOutcome::Timeout => Some("Compose config inspection timed out".into()),
            ProcOutcome::Spawn | ProcOutcome::Wait | ProcOutcome::Read => {
                Some("Compose config inspection could not run".into())
            }
            ProcOutcome::Exited { .. } => Some("Compose config inspection failed".into()),
        },
    };
    run_up_checked(up, ps, timeouts.1, timeouts.2)?;
    Ok(EnsureReport {
        compose_unverified: verified,
    })
}

/// Start the declared project and verify its requested service is running.
pub(crate) fn up_and_verify(
    spec: &SandboxSpec,
    compose: &ComposeSpec,
    guard: &ComposeGuard,
) -> anyhow::Result<EnsureReport> {
    let target = ComposeTarget::for_spec(spec)?;
    let state_home = crate::util::xdg_state_home();
    let project = probe_project(&state_home, spec, &target);
    let labelled = if matches!(&spec.placement, Placement::Local) {
        let existing_path = state_home
            .join("thegn/compose-labels")
            .join(format!("{project}.json"));
        let existing = match std::fs::symlink_metadata(&existing_path) {
            Ok(metadata) if metadata.file_type().is_file() => Some(existing_path),
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::debug!(target: "thegn::sandbox", %error, "unable to inspect existing Compose owner-label override");
                None
            }
        };
        let services = enumerate_services(&target, &services_argv(&target, &project, compose));
        labelled_compose(
            compose,
            services.as_deref(),
            &owner_identity(),
            existing,
            |body| {
                let directory = crate::fsperm::ensure_private_dir_beneath(
                    &state_home,
                    std::path::Path::new("thegn/compose-labels"),
                )?;
                directory.write_owner_only_atomic(&format!("{project}.json"), body.as_bytes())
            },
        )
    } else {
        compose.clone()
    };
    if !matches!(&spec.placement, Placement::Local) {
        let up = target.control_argv(&up_argv(&target, &project, &labelled));
        let ps = target.control_argv(&ps_argv(&target, &project, &labelled));
        run_up_checked(&up, &ps, COMPOSE_UP_TIMEOUT, COMPOSE_PS_TIMEOUT)?;
        return Ok(EnsureReport {
            compose_unverified: Some(
                "Compose placement is remote; local protected paths were not checked".into(),
            ),
        });
    }
    let config = target.control_argv(&config_json_argv(&target, &project, &labelled));
    let up = target.control_argv(&up_argv(&target, &project, &labelled));
    let ps = target.control_argv(&ps_argv(&target, &project, &labelled));
    up_attested(
        &config,
        &up,
        &ps,
        guard,
        (
            COMPOSE_CONFIG_TIMEOUT,
            COMPOSE_UP_TIMEOUT,
            COMPOSE_PS_TIMEOUT,
        ),
    )
}

/// `docker compose … exec [-it] [--workdir wd] <service> /bin/sh -lc <script>`.
/// Pure. `interactive` toggles `-it` (a pane) vs a one-shot exec.
pub(crate) fn exec_argv(
    target: &ComposeTarget,
    project: &str,
    c: &ComposeSpec,
    workdir: Option<&str>,
    script: &str,
    interactive: bool,
) -> Option<Vec<String>> {
    let service = c.service.as_ref()?;
    let mut v = project_prefix(target, project, c);
    v.push("exec".into());
    if interactive {
        v.push("-it".into());
    }
    if let Some(wd) = workdir {
        v.push("--workdir".into());
        v.push(wd.to_string());
    }
    v.push(service.clone());
    v.extend(["/bin/sh".into(), "-lc".into(), script.to_string()]);
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_error_display_covers_every_variant() {
        assert_eq!(
            ComposeError::Unsupported { backend: "apple" }.to_string(),
            "Compose is unsupported for backend 'apple'"
        );
        assert_eq!(
            ComposeError::Spawn.to_string(),
            "unable to run Compose command"
        );
        assert_eq!(
            ComposeError::Timeout.to_string(),
            "Compose command timed out"
        );
        assert_eq!(
            ComposeError::Exit {
                code: Some(2),
                stderr: "  boom  ".into(),
            }
            .to_string(),
            "Compose exited with status Some(2): \"boom\""
        );
        assert_eq!(
            ComposeError::OutputLimit.to_string(),
            "Compose output exceeded its size limit"
        );
        assert_eq!(
            ComposeError::NotRunning.to_string(),
            "Compose started no running service"
        );
    }

    #[test]
    fn legacy_single_file_roundtrips_as_bare_path() {
        let c = ComposeSpec {
            files: vec!["docker-compose.yml".into()],
            ..Default::default()
        };
        assert_eq!(c.encode(), "docker-compose.yml");
        assert_eq!(ComposeSpec::decode("docker-compose.yml"), c);
        assert!(!c.has_service());
    }

    #[test]
    fn full_spec_roundtrips() {
        let c = ComposeSpec {
            files: vec!["/a/compose.yml".into(), "/a/override.yml".into()],
            service: Some("app".into()),
            run_services: vec!["db".into(), "redis".into()],
        };
        let enc = c.encode();
        assert_eq!(ComposeSpec::decode(&enc), c);
        assert!(c.has_service());
    }

    #[test]
    fn service_only_no_runservices_roundtrips() {
        let c = ComposeSpec {
            files: vec!["c.yml".into()],
            service: Some("web".into()),
            run_services: vec![],
        };
        assert_eq!(ComposeSpec::decode(&c.encode()), c);
    }

    #[test]
    fn up_argv_includes_files_project_and_services() {
        let target = ComposeTarget {
            engine: vec!["docker".into(), "compose".into()],
            placement: Placement::Local,
        };
        let c = ComposeSpec {
            files: vec!["/a/c.yml".into(), "/a/o.yml".into()],
            service: Some("app".into()),
            run_services: vec!["db".into()],
        };
        assert_eq!(
            up_argv(&target, "proj", &c),
            vec![
                "docker", "compose", "-f", "/a/c.yml", "-f", "/a/o.yml", "-p", "proj", "up", "-d",
                "app", "db",
            ]
        );
    }

    #[test]
    fn owner_label_override_labels_every_service_deterministically() {
        let services = vec!["web".into(), "db-1".into(), "bad/name".into()];
        let body = owner_label_override(&services, "profile.hash").expect("valid services");
        assert_eq!(
            body,
            r#"{"services":{"db-1":{"labels":{"thegn.managed":"true","thegn.owner":"profile.hash"}},"web":{"labels":{"thegn.managed":"true","thegn.owner":"profile.hash"}}}}"#
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(
            parsed["services"]["web"]["labels"][OWNED_LABEL.split_once('=').unwrap().0],
            "true"
        );
        assert_eq!(
            parsed["services"]["web"]["labels"][OWNER_LABEL_KEY],
            "profile.hash"
        );
        assert!(parsed["services"].get("bad/name").is_none());
        assert_eq!(owner_label_override(&["bad/name".into()], "owner"), None);
        assert_eq!(owner_label_override(&[], "owner"), None);
    }

    #[test]
    fn services_argv_lists_compose_services() {
        let target = ComposeTarget {
            engine: vec!["docker".into(), "compose".into()],
            placement: Placement::Local,
        };
        let compose = ComposeSpec {
            files: vec!["base.yml".into(), "dev.yml".into()],
            ..Default::default()
        };
        assert_eq!(
            services_argv(&target, "proj", &compose),
            vec![
                "docker",
                "compose",
                "-f",
                "base.yml",
                "-f",
                "dev.yml",
                "-p",
                "proj",
                "config",
                "--services"
            ]
        );
    }

    #[test]
    fn config_json_argv_uses_the_project_files_and_effective_json_format() {
        let target = ComposeTarget {
            engine: vec!["docker".into(), "compose".into()],
            placement: Placement::Local,
        };
        let compose = ComposeSpec {
            files: vec!["base.yml".into(), "labels.json".into()],
            service: Some("web".into()),
            run_services: vec!["db".into()],
        };
        assert_eq!(
            config_json_argv(&target, "proj", &compose),
            vec![
                "docker",
                "compose",
                "-f",
                "base.yml",
                "-f",
                "labels.json",
                "-p",
                "proj",
                "config",
                "--format",
                "json",
                "web",
                "db"
            ]
        );
    }

    #[test]
    fn up_argv_with_override_puts_label_file_last() {
        let target = ComposeTarget {
            engine: vec!["docker".into(), "compose".into()],
            placement: Placement::Local,
        };
        let compose = ComposeSpec {
            files: vec!["base.yml".into(), "dev.yml".into()],
            ..Default::default()
        };
        let mut labelled = compose.clone();
        labelled.files.push("/state/proj.json".into());
        let argv = up_argv(&target, "proj", &labelled);
        assert_eq!(
            &argv[..10],
            &[
                "docker",
                "compose",
                "-f",
                "base.yml",
                "-f",
                "dev.yml",
                "-f",
                "/state/proj.json",
                "-p",
                "proj"
            ]
        );
        assert_eq!(argv.last().map(String::as_str), Some("-d"));
    }

    #[test]
    fn labelled_compose_writes_override_and_fails_open_to_unlabelled() {
        let compose = ComposeSpec {
            files: vec!["base.yml".into()],
            ..Default::default()
        };
        let mut written = None;
        let labelled = labelled_compose(&compose, Some("web\ndb\n"), "owner", None, |body| {
            written = Some(body.to_owned());
            Ok(PathBuf::from("/private/proj.json"))
        });
        assert_eq!(labelled.files, vec!["base.yml", "/private/proj.json"]);
        let json: serde_json::Value =
            serde_json::from_str(&written.expect("writer called")).expect("valid JSON");
        assert_eq!(json["services"]["web"]["labels"][OWNER_LABEL_KEY], "owner");

        let unchanged = labelled_compose(&compose, Some("web\n"), "owner", None, |_| {
            Err(std::io::Error::other("write denied"))
        });
        assert_eq!(unchanged, compose);
        let unenumerated = labelled_compose(&compose, None, "owner", None, |_| {
            panic!("writer must not run without service enumeration")
        });
        assert_eq!(unenumerated, compose);
    }

    #[test]
    fn labelled_compose_reuses_existing_override_on_transient_failures() {
        let compose = ComposeSpec {
            files: vec!["base.yml".into()],
            ..Default::default()
        };
        let existing = PathBuf::from("/private/proj.json");
        let expected = ComposeSpec {
            files: vec!["base.yml".into(), "/private/proj.json".into()],
            ..Default::default()
        };

        let unenumerated =
            labelled_compose(&compose, None, "owner", Some(existing.clone()), |_| {
                panic!("writer must not run without service enumeration")
            });
        assert_eq!(unenumerated, expected);

        let write_failed =
            labelled_compose(&compose, Some("web\n"), "owner", Some(existing), |_| {
                Err(std::io::Error::other("transient write failure"))
            });
        assert_eq!(write_failed, expected);
    }

    #[test]
    fn plan_gc_removes_labelled_compose_replica_of_dead_worktree() {
        let all = vec!["thegn-dead-db-1".to_string()];
        let mine = all.clone();
        let owned = crate::sandbox_manage::plan_gc(&[], &all, Some(&mine), None);
        assert_eq!(owned.remove, all);
        assert!(owned.unverified.is_empty());

        let unlabelled = crate::sandbox_manage::plan_gc(&[], &all, None, None);
        assert!(unlabelled.remove.is_empty());
        assert_eq!(unlabelled.unverified, all);
    }

    #[test]
    fn stale_label_sweep_removes_old_json_files() {
        let anchor = tempfile::tempdir().expect("private temp anchor");
        let directory = crate::fsperm::ensure_private_dir_beneath(
            anchor.path(),
            std::path::Path::new("thegn/compose-labels"),
        )
        .expect("private label directory");
        let path = directory
            .write_owner_only_atomic("proj.json", b"{}")
            .expect("write label file");
        let removed = sweep_stale_label_files_in(
            anchor.path(),
            std::time::SystemTime::now() + Duration::from_secs(2),
            Duration::from_secs(1),
        );
        assert_eq!(removed, 1);
        assert!(!path.exists());
    }

    #[test]
    fn ps_argv_includes_files_project_and_service() {
        let target = ComposeTarget {
            engine: vec!["docker".into(), "compose".into()],
            placement: Placement::Local,
        };
        let c = ComposeSpec {
            files: vec!["/a/c.yml".into(), "/a/o.yml".into()],
            service: Some("app".into()),
            run_services: vec!["db".into()],
        };
        assert_eq!(
            ps_argv(&target, "proj", &c),
            vec![
                "docker", "compose", "-f", "/a/c.yml", "-f", "/a/o.yml", "-p", "proj", "ps",
                "--status", "running", "--quiet", "app",
            ]
        );
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[cfg(unix)]
    #[test]
    fn compose_up_timeout_is_bounded() {
        let started = std::time::Instant::now();
        let result = run_up_checked(
            &shell("sleep 5"),
            &shell("echo abc123"),
            Duration::from_millis(200),
            Duration::from_secs(1),
        );
        assert_eq!(result, Err(ComposeError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn compose_up_reaps_descendant_holding_capture_pipe_by_deadline() {
        let started = std::time::Instant::now();
        let result = run_up_checked(
            &shell("sleep 30 & exit 0"),
            &shell("echo abc123"),
            Duration::from_millis(300),
            Duration::from_secs(1),
        );
        assert!(result.is_ok());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn compose_up_reports_nonzero_status_and_bounded_stderr() {
        let result = run_up_checked(
            &shell("echo boom >&2; exit 3"),
            &shell("echo unused"),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert_eq!(
            result,
            Err(ComposeError::Exit {
                code: Some(3),
                stderr: "boom\n".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn compose_up_spawn_failure_is_reported() {
        assert_eq!(
            run_up_checked(
                &["/nonexistent/thegn-compose-test".into()],
                &shell("echo x"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(ComposeError::Spawn)
        );
    }

    #[cfg(unix)]
    #[test]
    fn compose_ps_spawn_failure_is_reported() {
        assert_eq!(
            run_up_checked(
                &shell("true"),
                &["/nonexistent/thegn-compose-test".into()],
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(ComposeError::Spawn)
        );
    }

    #[cfg(unix)]
    #[test]
    fn compose_ps_reports_exit_status_and_stderr() {
        assert_eq!(
            run_up_checked(
                &shell("true"),
                &shell("echo ps-failed >&2; exit 2"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(ComposeError::Exit {
                code: Some(2),
                stderr: "ps-failed\n".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn compose_up_truncated_stderr_preserves_failure_status() {
        assert_eq!(
            run_up_checked(
                &shell("head -c 70000 /dev/zero >&2; exit 7"),
                &shell("echo unused"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(ComposeError::Exit {
                code: Some(7),
                stderr: "<stderr truncated>".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn compose_up_requires_running_service_and_rejects_oversized_ps_output() {
        assert_eq!(
            run_up_checked(
                &shell("true"),
                &shell("true"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(ComposeError::NotRunning)
        );
        assert_eq!(
            run_up_checked(
                &shell("true"),
                &shell("echo abc123"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Ok(())
        );
        assert_eq!(
            run_up_checked(
                &shell("true"),
                &shell("head -c 5000 /dev/zero"),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(ComposeError::OutputLimit)
        );
    }

    #[test]
    fn exec_argv_interactive_with_workdir() {
        let target = ComposeTarget {
            engine: vec!["docker".into(), "compose".into()],
            placement: Placement::Local,
        };
        let c = ComposeSpec {
            files: vec!["/a/c.yml".into()],
            service: Some("app".into()),
            run_services: vec![],
        };
        let argv = exec_argv(&target, "proj", &c, Some("/work"), "exec sh", true).unwrap();
        assert_eq!(
            argv,
            vec![
                "docker",
                "compose",
                "-f",
                "/a/c.yml",
                "-p",
                "proj",
                "exec",
                "-it",
                "--workdir",
                "/work",
                "app",
                "/bin/sh",
                "-lc",
                "exec sh",
            ]
        );
    }

    #[test]
    fn exec_argv_none_without_service() {
        let target = ComposeTarget {
            engine: vec!["docker".into(), "compose".into()],
            placement: Placement::Local,
        };
        let c = ComposeSpec {
            files: vec!["c.yml".into()],
            ..Default::default()
        };
        assert!(exec_argv(&target, "proj", &c, None, "s", true).is_none());
    }

    fn protected_json(volume: serde_json::Value) -> String {
        serde_json::json!({"services":{"worker":{"volumes":[volume]}}}).to_string()
    }

    #[test]
    fn effective_compose_binds_refuse_protected_sources_and_accept_worktree_and_named_volume() {
        let root = tempfile::tempdir().expect("private tempdir");
        let state = root.path().join("state/thegn");
        std::fs::create_dir_all(&state).unwrap();
        let protected = vec![state.clone()];
        let check = |json: String| protected_binds(&json, &protected).unwrap();
        let long = protected_json(
            serde_json::json!({"type":"bind","source":state.to_string_lossy().into_owned(),"target":"/x"}),
        );
        assert!(check(long).is_some(), "long bind must be refused");
        let short = serde_json::json!({"services":{"worker":{"volumes":[format!("{}:/secret:ro", state.display())]}}}).to_string();
        assert!(check(short).is_some(), "short bind must be refused");
        let ancestor = protected_json(
            serde_json::json!({"type":"bind","source":root.path().to_string_lossy().into_owned(),"target":"/all"}),
        );
        assert!(check(ancestor).is_some(), "ancestor bind must be refused");

        let volume = serde_json::json!({
            "services":{"worker":{"volumes":[{"type":"volume","source":"hostdata","target":"/x"}]}},
            "volumes":{"hostdata":{"driver":"local","driver_opts":{"type":"none","o":"bind","device":state.to_string_lossy().into_owned()}}}
        }).to_string();
        assert!(check(volume).is_some(), "driver_opts bind must be refused");
        let rbind_volume = serde_json::json!({
            "services":{"worker":{"volumes":[{"type":"volume","source":"hostdata","target":"/x"}]}},
            "volumes":{"hostdata":{"driver":"local","driver_opts":{"type":"none","o":"  rbind , rw ","device":state.to_string_lossy().into_owned()}}}
        }).to_string();
        assert!(
            check(rbind_volume).is_some(),
            "driver_opts rbind must be refused"
        );
        for section in ["secrets", "configs"] {
            let mut config = serde_json::json!({"services":{"worker":{}}});
            config[section] =
                serde_json::json!({"auth":{"file":state.to_string_lossy().into_owned()}});
            config["services"]["worker"][section] = serde_json::json!(["auth"]);
            assert!(
                check(config.to_string()).is_some(),
                "file-backed {section} must be refused"
            );
        }
        assert!(
            check(protected_json(
                serde_json::json!({"type":"bind","source":"/work/tree","target":"/src"})
            ))
            .is_none()
        );
        assert!(
            check(
                serde_json::json!({"services":{"worker":{"volumes":["cache:/cache"]}}}).to_string()
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_credentials_and_engine_socket_sources_are_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("private tempdir");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let credential = root.path().join("credential");
        std::fs::create_dir(&credential).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&credential, &alias).unwrap();
        let symlink =
            protected_json(serde_json::json!({"type":"bind","source":alias,"target":"/x"}));
        assert!(
            protected_binds(&symlink, std::slice::from_ref(&credential))
                .unwrap()
                .is_some()
        );
        for name in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/gh",
            ".kube",
            ".docker",
            ".netrc",
            ".git-credentials",
        ] {
            let path = root.path().join(name);
            let config = protected_json(
                serde_json::json!({"type":"bind","source":path,"target":"/credentials"}),
            );
            assert!(
                protected_binds(&config, &[root.path().join(name)])
                    .unwrap()
                    .is_some(),
                "{name} must be refused"
            );
        }
        for socket in [
            "/somewhere/docker.sock",
            "/somewhere/podman.sock",
            "/somewhere/containerd.sock",
        ] {
            let config = protected_json(
                serde_json::json!({"type":"bind","source":socket,"target":"/run/engine.sock"}),
            );
            assert!(
                protected_binds(&config, &[]).unwrap().is_some(),
                "{socket} must be refused"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn up_attested_refuses_before_up_and_launches_unverified_when_inspection_fails() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("private tempdir");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let marker = root.path().join("up-ran");
        let config_json = serde_json::json!({"services":{"worker":{"volumes":[{"type":"bind","source":"/protected/state","target":"/state"}]}}}).to_string();
        let config_script = format!("printf '%s' '{}'", config_json.replace('\'', "'\\''"));
        let up_script = format!("touch '{}'", marker.display());
        let ps = shell("printf 'container-id\\n'");
        let guard = ComposeGuard {
            protected: vec![PathBuf::from("/protected/state")],
            classes: vec!["thegn state".into()],
        };
        let result = up_attested(
            &shell(&config_script),
            &shell(&up_script),
            &ps,
            &guard,
            (
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(2),
            ),
        );
        let refusal = result.unwrap_err();
        let refusal = refusal
            .downcast_ref::<ComposeBindRefusal>()
            .expect("protected bind refusal type");
        assert!(refusal.to_string().contains("thegn state"));
        assert!(!marker.exists(), "up must not run after refusal");
        let failed = up_attested(
            &shell("exit 1"),
            &shell(&up_script),
            &ps,
            &guard,
            (
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(2),
            ),
        )
        .unwrap();
        assert!(failed.compose_unverified.is_some());
        assert!(marker.exists(), "unavailable inspection permits launch");
        std::fs::remove_file(&marker).unwrap();
        let started = std::time::Instant::now();
        let timed = up_attested(
            &shell("sleep 5"),
            &shell(&up_script),
            &ps,
            &guard,
            (
                Duration::from_millis(200),
                Duration::from_secs(2),
                Duration::from_secs(2),
            ),
        )
        .unwrap();
        assert!(timed.compose_unverified.is_some());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn successful_but_unverifiable_config_refuses_before_up() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("private tempdir");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let marker = root.path().join("up-ran");
        let up_script = format!("touch '{}'", marker.display());
        let ps = shell("printf 'container-id\\n'");
        let guard = ComposeGuard {
            protected: Vec::new(),
            ..Default::default()
        };
        for (config_script, expected) in [
            ("printf 'not-json'".to_owned(), "could not be verified"),
            ("head -c 1048577 /dev/zero".to_owned(), "size limit"),
        ] {
            let error = up_attested(
                &shell(&config_script),
                &shell(&up_script),
                &ps,
                &guard,
                (
                    Duration::from_secs(2),
                    Duration::from_secs(2),
                    Duration::from_secs(2),
                ),
            )
            .expect_err("successful config output that cannot be checked must refuse");
            assert!(error.to_string().contains(expected), "{error:#}");
            assert!(
                !marker.exists(),
                "up must not run after verification failure"
            );
        }
    }

    #[test]
    fn unverified_compose_is_reported_as_host_process() {
        let base = crate::capabilities::IsolationClass::SharedKernel;
        assert_eq!(
            compose_reported_isolation(base, &EnsureReport::default()),
            base
        );
        assert_eq!(
            compose_reported_isolation(
                base,
                &EnsureReport {
                    compose_unverified: Some("timeout".into())
                }
            ),
            crate::capabilities::IsolationClass::HostProcess
        );
    }

    #[test]
    fn hashed_project_name_is_path_specific_stable_and_keeps_prefix() {
        let name = "thegn-w-my-repo";
        let first = hashed_project_name(name, Path::new("/w/my.repo"));
        let second = hashed_project_name(name, Path::new("/w/my-repo"));
        assert_ne!(first, second);
        assert_eq!(first, hashed_project_name(name, Path::new("/w/my.repo")));
        assert!(first.starts_with("thegn-"));
        assert!(
            first
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        );
        assert!(first.rsplit('-').next().unwrap().len() == 6);
        assert!(
            first
                .rsplit('-')
                .next()
                .unwrap()
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
    }

    #[test]
    fn legacy_working_directory_uses_component_prefixes() {
        assert!(legacy_owned_by(
            "/w/my.repo/.devcontainer\n",
            Path::new("/w/my.repo")
        ));
        assert!(!legacy_owned_by(
            "/w/my-repo/.devcontainer\n",
            Path::new("/w/my.repo")
        ));
        assert!(!legacy_owned_by("relative/path\n", Path::new("/w/my.repo")));
    }
}
