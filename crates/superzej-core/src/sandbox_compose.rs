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

/// The `docker compose` binary tokens. Compose v2 (the `docker compose`
/// subcommand) is the current standard; kept as one place to change.
fn compose_bin() -> Vec<String> {
    vec!["docker".into(), "compose".into()]
}

/// `docker compose -f … -p <project> …<rest>` prefix shared by up/exec.
fn project_prefix(project: &str, c: &ComposeSpec) -> Vec<String> {
    let mut v = compose_bin();
    for f in &c.files {
        v.push("-f".into());
        v.push(f.clone());
    }
    v.push("-p".into());
    v.push(project.to_string());
    v
}

/// `docker compose … up -d [service runServices…]`. Pure.
pub fn up_argv(project: &str, c: &ComposeSpec) -> Vec<String> {
    let mut v = project_prefix(project, c);
    v.push("up".into());
    v.push("-d".into());
    if let Some(svc) = &c.service {
        v.push(svc.clone());
    }
    v.extend(c.run_services.iter().cloned());
    v
}

/// `docker compose … exec [-it] [--workdir wd] <service> /bin/sh -lc <script>`.
/// Pure. `interactive` toggles `-it` (a pane) vs a one-shot exec.
pub fn exec_argv(
    project: &str,
    c: &ComposeSpec,
    workdir: Option<&str>,
    script: &str,
    interactive: bool,
) -> Option<Vec<String>> {
    let service = c.service.as_ref()?;
    let mut v = project_prefix(project, c);
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
        let c = ComposeSpec {
            files: vec!["/a/c.yml".into(), "/a/o.yml".into()],
            service: Some("app".into()),
            run_services: vec!["db".into()],
        };
        assert_eq!(
            up_argv("proj", &c),
            vec![
                "docker", "compose", "-f", "/a/c.yml", "-f", "/a/o.yml", "-p", "proj", "up", "-d",
                "app", "db",
            ]
        );
    }

    #[test]
    fn exec_argv_interactive_with_workdir() {
        let c = ComposeSpec {
            files: vec!["/a/c.yml".into()],
            service: Some("app".into()),
            run_services: vec![],
        };
        let argv = exec_argv("proj", &c, Some("/work"), "exec sh", true).unwrap();
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
        let c = ComposeSpec {
            files: vec!["c.yml".into()],
            ..Default::default()
        };
        assert!(exec_argv("proj", &c, None, "s", true).is_none());
    }
}
