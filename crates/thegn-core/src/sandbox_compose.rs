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
use std::path::PathBuf;
use std::time::Duration;

/// Maximum time allowed for `docker compose up`, including any pull it triggers.
pub(crate) const COMPOSE_UP_TIMEOUT: Duration = Duration::from_secs(150);
/// Maximum time allowed to verify that Compose reports a running service.
pub(crate) const COMPOSE_PS_TIMEOUT: Duration = Duration::from_secs(5);
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

/// `docker compose … config --services`. Pure; used to enumerate precisely
/// which services will receive the owner-label override.
pub(crate) fn services_argv(target: &ComposeTarget, project: &str, c: &ComposeSpec) -> Vec<String> {
    let mut argv = project_prefix(target, project, c);
    argv.extend(["config".into(), "--services".into()]);
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
    write: impl FnOnce(&str) -> std::io::Result<PathBuf>,
) -> ComposeSpec {
    let Some(services_stdout) = services_stdout else {
        return compose.clone();
    };
    let services = services_stdout
        .lines()
        .map(str::trim)
        .filter(|service| !service.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let Some(body) = owner_label_override(&services, owner) else {
        return compose.clone();
    };
    match write(&body) {
        Ok(path) => {
            let mut labelled = compose.clone();
            labelled.files.push(path.to_string_lossy().into_owned());
            labelled
        }
        Err(error) => {
            tracing::info!(target: "thegn::sandbox", %error, "unable to write Compose owner-label override; starting unlabelled");
            compose.clone()
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
            tracing::info!(target: "thegn::sandbox", "unable to enumerate Compose services for owner labels; starting unlabelled");
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

/// Start the declared project and verify its requested service is running.
pub(crate) fn up_and_verify(spec: &SandboxSpec, compose: &ComposeSpec) -> anyhow::Result<()> {
    let target = ComposeTarget::for_spec(spec)?;
    let labelled = if matches!(&spec.placement, Placement::Local) {
        let services = enumerate_services(&target, &services_argv(&target, &spec.name, compose));
        labelled_compose(compose, services.as_deref(), &owner_identity(), |body| {
            let directory = crate::fsperm::ensure_private_dir_beneath(
                &crate::util::xdg_state_home(),
                std::path::Path::new("thegn/compose-labels"),
            )?;
            directory.write_owner_only_atomic(&format!("{}.json", spec.name), body.as_bytes())
        })
    } else {
        compose.clone()
    };
    let up = target.control_argv(&up_argv(&target, &spec.name, &labelled));
    let ps = target.control_argv(&ps_argv(&target, &spec.name, &labelled));
    run_up_checked(&up, &ps, COMPOSE_UP_TIMEOUT, COMPOSE_PS_TIMEOUT)?;
    Ok(())
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
        let labelled = labelled_compose(&compose, Some("web\ndb\n"), "owner", |body| {
            written = Some(body.to_owned());
            Ok(PathBuf::from("/private/proj.json"))
        });
        assert_eq!(labelled.files, vec!["base.yml", "/private/proj.json"]);
        let json: serde_json::Value =
            serde_json::from_str(&written.expect("writer called")).expect("valid JSON");
        assert_eq!(json["services"]["web"]["labels"][OWNER_LABEL_KEY], "owner");

        let unchanged = labelled_compose(&compose, Some("web\n"), "owner", |_| {
            Err(std::io::Error::other("write denied"))
        });
        assert_eq!(unchanged, compose);
        let unenumerated = labelled_compose(&compose, None, "owner", |_| {
            panic!("writer must not run without service enumeration")
        });
        assert_eq!(unenumerated, compose);
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
}
