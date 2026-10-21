//! Ownership fencing for devcontainer-declared named OCI volumes.
//!
//! Docker and Podman report absent volumes with, among others, Docker's
//! `Error response from daemon: get NAME: no such volume` and `Error: No such
//! volume: NAME`, and Podman's `Error: inspecting object: no such volume NAME`
//! and `Error: no such volume NAME`. A Go-template `index` of a missing label
//! prints an empty line; Docker may print `<no value>` for a nil Labels map.
//! Legacy unlabelled volumes are claimed through an owner-only local record.

use crate::sandbox::{self, Backend, EnterError, RefusalClass, SandboxSpec};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const VOLUME_OWNER_LABEL: &str = "thegn.volume.owner";
pub const DEVCONTAINER_VOLUME_ROLE: &str = "devcontainer";

/// Success flag and bounded stdout/stderr from one volume command.
pub type VolumeCommandOutput = (bool, String, String);
/// Callback used to run a single bounded volume command.
pub type VolumeCommandRunner = dyn FnMut(&[String]) -> Option<VolumeCommandOutput>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeProbe {
    Absent,
    Labelled(String),
    Unlabelled,
    Unknown(String),
}

/// Parse Docker/Podman label probes. Unknown output is bounded and control-redacted.
pub fn parse_volume_probe(ok: bool, stdout: &str, stderr: &str) -> VolumeProbe {
    if ok {
        let value = stdout.trim();
        if value.is_empty() || value == "<no value>" {
            VolumeProbe::Unlabelled
        } else {
            VolumeProbe::Labelled(value.to_string())
        }
    } else if stderr.to_ascii_lowercase().contains("no such volume") {
        VolumeProbe::Absent
    } else {
        let safe = redacted_tail(stderr);
        VolumeProbe::Unknown(if safe.is_empty() {
            "volume probe failed".into()
        } else {
            safe
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimPlan {
    UseLabelled,
    Create,
    ClaimLegacy,
    UseClaimed,
    Refuse(String),
}

pub fn plan_claim(probe: &VolumeProbe, owner: &str, claim_record: Option<&str>) -> ClaimPlan {
    match probe {
        VolumeProbe::Absent => ClaimPlan::Create,
        VolumeProbe::Labelled(value) if value == owner => ClaimPlan::UseLabelled,
        VolumeProbe::Labelled(_) => ClaimPlan::Refuse("owned by another devcontainer".into()),
        VolumeProbe::Unlabelled => match claim_record {
            None => ClaimPlan::ClaimLegacy,
            Some(value) if value == owner => ClaimPlan::UseClaimed,
            Some(_) => ClaimPlan::Refuse("legacy volume is claimed by another devcontainer".into()),
        },
        VolumeProbe::Unknown(error) => ClaimPlan::Refuse(error.clone()),
    }
}

pub fn volume_probe_argv(prefix: &[String], name: &str) -> Vec<String> {
    let mut argv = prefix.to_vec();
    argv.extend([
        "volume".into(),
        "inspect".into(),
        "--format".into(),
        format!("{{{{index .Labels \"{VOLUME_OWNER_LABEL}\"}}}}"),
        name.into(),
    ]);
    argv
}

pub fn volume_create_argv(prefix: &[String], name: &str, owner: &str) -> Vec<String> {
    let mut argv = prefix.to_vec();
    argv.extend([
        "volume".into(),
        "create".into(),
        "--label".into(),
        "thegn.managed=true".into(),
        "--label".into(),
        format!("thegn.volume.role={DEVCONTAINER_VOLUME_ROLE}"),
        "--label".into(),
        format!("{VOLUME_OWNER_LABEL}={owner}"),
        name.into(),
    ]);
    argv
}

fn claim_dir(state_home: &Path) -> PathBuf {
    state_home.join("thegn/devcontainer-volumes")
}

fn record_path(spec: &SandboxSpec, dir: &Path, name: &str) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(b"thegn.devcontainer-volume-claim.v1\0");
    hash.update(spec.backend.label().as_bytes());
    hash.update(b"\0");
    hash.update(spec.oci_host.as_deref().unwrap_or("").as_bytes());
    hash.update(b"\0");
    hash.update(name.as_bytes());
    let hex = format!("{:x}", hash.finalize());
    dir.join(&hex[..32])
}

fn read_claim(path: &Path) -> std::io::Result<Option<String>> {
    match crate::fsperm::open_regular_file_nofollow(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(257).read_to_end(&mut bytes)?;
            if bytes.len() > 256 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "volume claim exceeds limit",
                ));
            }
            let value = std::str::from_utf8(&bytes).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "volume claim is not UTF-8")
            })?;
            let Some(owner) = value.strip_suffix('\n') else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "volume claim is malformed",
                ));
            };
            Ok(Some(owner.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_claim(path: &Path, owner: &str) -> std::io::Result<()> {
    let mut file = crate::fsperm::create_owner_only_exclusive(path)?;
    use std::io::Write;
    file.write_all(format!("{owner}\n").as_bytes())?;
    file.sync_all()
}

fn claim_legacy_record(path: &Path, owner: &str) -> Result<(), String> {
    match write_claim(path, owner) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let record = read_claim(path).map_err(|read_error| {
                format!("claim record unreadable after race: {read_error}")
            })?;
            match plan_claim(&VolumeProbe::Unlabelled, owner, record.as_deref()) {
                ClaimPlan::UseClaimed => Ok(()),
                ClaimPlan::Refuse(reason) => Err(reason),
                ClaimPlan::ClaimLegacy | ClaimPlan::Create | ClaimPlan::UseLabelled => {
                    Err("legacy volume claim raced with an unreadable owner".into())
                }
            }
        }
        Err(error) => Err(format!("claim record write failed: {error}")),
    }
}

fn refused(spec: &SandboxSpec, name: &str, why: &str) -> EnterError {
    sandbox::refusal(
        spec,
        RefusalClass::VolumeOwner,
        None,
        format!(
            "devcontainer volume '{}' refused: {why}",
            bounded_name(name)
        ),
    )
}

fn bounded_name(name: &str) -> String {
    name.chars().take(96).collect()
}

fn redacted_tail(value: &str) -> String {
    let tail = value
        .chars()
        .rev()
        .take(256)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let redacted = crate::log_redact::redact_text_line(&tail);
    redacted
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Check, create or claim all devcontainer volumes before container creation.
// A generic runner lets callers borrow launch state such as `spec`.
pub fn claim_volumes<F>(
    spec: &SandboxSpec,
    state_home: &Path,
    run: &mut F,
) -> Result<(), EnterError>
where
    F: FnMut(&[String]) -> Option<VolumeCommandOutput> + ?Sized,
{
    if spec.volume_owners.is_empty() {
        return Ok(());
    }
    let dir = claim_dir(state_home);
    crate::fsperm::ensure_private_dir(&dir).map_err(|error| {
        refused(
            spec,
            "<state>",
            &format!("claim directory unavailable: {error}"),
        )
    })?;
    let prefix = sandbox::oci_prefix(spec);
    for (name, owner) in &spec.volume_owners {
        let path = record_path(spec, &dir, name);
        let record = if spec.placement.is_local() {
            read_claim(&path).map_err(|error| {
                refused(spec, name, &format!("claim record unreadable: {error}"))
            })?
        } else {
            None
        };
        if spec.backend == Backend::Apple && spec.placement.is_local() {
            match plan_claim(&VolumeProbe::Unlabelled, owner, record.as_deref()) {
                ClaimPlan::ClaimLegacy => {
                    claim_legacy_record(&path, owner)
                        .map_err(|error| refused(spec, name, &error))?;
                    continue;
                }
                ClaimPlan::UseClaimed => continue,
                ClaimPlan::Refuse(why) => return Err(refused(spec, name, &why)),
                ClaimPlan::Create | ClaimPlan::UseLabelled => {
                    unreachable!("unlabelled Apple volume plan")
                }
            }
        }
        let probe_argv = volume_probe_argv(&prefix, name);
        let Some((ok, stdout, stderr)) = run(&probe_argv) else {
            return Err(refused(
                spec,
                name,
                "volume probe timed out or could not start",
            ));
        };
        let probe = parse_volume_probe(ok, &stdout, &stderr);
        if !spec.placement.is_local() && matches!(&probe, VolumeProbe::Unlabelled) {
            return Err(refused(
                spec,
                name,
                "unlabelled volume on a remote host: recreate it or remove the devcontainer mount",
            ));
        }
        match plan_claim(&probe, owner, record.as_deref()) {
            ClaimPlan::UseLabelled | ClaimPlan::UseClaimed => continue,
            ClaimPlan::Refuse(why) => return Err(refused(spec, name, &why)),
            ClaimPlan::ClaimLegacy if !spec.placement.is_local() => {
                return Err(refused(
                    spec,
                    name,
                    "unlabelled volume on a remote host: recreate it or remove the devcontainer mount",
                ));
            }
            ClaimPlan::ClaimLegacy => {
                claim_legacy_record(&path, owner).map_err(|error| refused(spec, name, &error))?;
                continue;
            }
            ClaimPlan::Create => {
                let create_argv = volume_create_argv(&prefix, name, owner);
                let Some((created, _, create_err)) = run(&create_argv) else {
                    return Err(refused(
                        spec,
                        name,
                        "volume create timed out or could not start",
                    ));
                };
                if created {
                    continue;
                }
                if !create_err.to_ascii_lowercase().contains("already exists") {
                    return Err(refused(
                        spec,
                        name,
                        &format!("volume create failed: {}", redacted_tail(&create_err)),
                    ));
                }
                let Some((retry_ok, retry_out, retry_err)) = run(&probe_argv) else {
                    return Err(refused(
                        spec,
                        name,
                        "volume re-probe timed out or could not start",
                    ));
                };
                match plan_claim(
                    &parse_volume_probe(retry_ok, &retry_out, &retry_err),
                    owner,
                    record.as_deref(),
                ) {
                    ClaimPlan::UseLabelled | ClaimPlan::UseClaimed => continue,
                    ClaimPlan::Refuse(why) => return Err(refused(spec, name, &why)),
                    ClaimPlan::ClaimLegacy if spec.placement.is_local() => {
                        claim_legacy_record(&path, owner)
                            .map_err(|error| refused(spec, name, &error))?;
                    }
                    _ => {
                        return Err(refused(
                            spec,
                            name,
                            "volume appeared without the expected owner label",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FileAccess, Network};
    use crate::placement::Placement;

    const OWNER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn spec(owner: &str) -> SandboxSpec {
        SandboxSpec {
            backend: Backend::Podman,
            placement: Placement::Local,
            image: None,
            worktree: PathBuf::from("/worktree"),
            mounts: Vec::new(),
            env: Vec::new(),
            env_overrides: Default::default(),
            env_block: Vec::new(),
            network: Network::Nat,
            network_allow: Vec::new(),
            network_block: Vec::new(),
            read_only_root: false,
            no_new_privileges: false,
            pids_limit: None,
            drop_capabilities: Vec::new(),
            add_capabilities: Vec::new(),
            file_access: FileAccess::default(),
            ports: Vec::new(),
            gpu: None,
            limits: Default::default(),
            volumes: vec![("data".into(), "/data".into())],
            compose: None,
            build: None,
            init_script: None,
            devenv: false,
            devenv_path: None,
            name: "test".into(),
            vpn: None,
            oci_host: None,
            oci_runtime: None,
            daemon_persistent: false,
            seal_home: None,
            volume_owners: vec![("data".into(), owner.into())],
        }
    }

    #[test]
    fn mismatched_owner_label_refuses() {
        assert!(matches!(
            plan_claim(
                &VolumeProbe::Labelled("b".repeat(64)),
                &"a".repeat(64),
                None
            ),
            ClaimPlan::Refuse(_)
        ));
    }

    #[test]
    fn absent_volume_is_created_with_owner_labels() {
        let argv = volume_create_argv(&["podman".into()], "data", OWNER);
        assert!(
            argv.windows(2)
                .any(|pair| { pair[0] == "--label" && pair[1] == "thegn.managed=true" })
        );
        assert!(
            argv.windows(2).any(|pair| {
                pair[0] == "--label" && pair[1] == "thegn.volume.role=devcontainer"
            })
        );
        let owner_label = format!("thegn.volume.owner={OWNER}");
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "--label" && pair[1] == owner_label)
        );
        assert_eq!(argv.last().map(String::as_str), Some("data"));
    }

    #[test]
    fn legacy_unlabelled_volume_is_claimed_then_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let mut creates = 0;
        let result = {
            let mut runner = |argv: &[String]| {
                if argv.iter().any(|arg| arg.as_str() == "create") {
                    creates += 1;
                }
                Some((true, "\n".into(), String::new()))
            };
            claim_volumes(&spec(&"a".repeat(64)), &state, &mut runner).unwrap();
            let entries = std::fs::read_dir(state.join("thegn/devcontainer-volumes"))
                .unwrap()
                .count();
            assert_eq!(entries, 1);
            claim_volumes(&spec(&"b".repeat(64)), &state, &mut runner)
        };
        assert!(matches!(
            result,
            Err(EnterError::Refused(crate::sandbox::Refusal {
                class: RefusalClass::VolumeOwner,
                ..
            }))
        ));
        assert_eq!(creates, 0);
    }

    #[test]
    fn probe_parses_real_engine_shapes() {
        let argv = volume_probe_argv(&["podman".into()], "data");
        assert_eq!(
            argv.iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "podman",
                "volume",
                "inspect",
                "--format",
                r#"{{index .Labels "thegn.volume.owner"}}"#,
                "data"
            ]
        );
        for stderr in [
            "Error response from daemon: get NAME: no such volume",
            "Error: No such volume: NAME",
            "Error: inspecting object: no such volume NAME",
            "Error: no such volume NAME",
        ] {
            assert_eq!(parse_volume_probe(false, "", stderr), VolumeProbe::Absent);
        }
        assert_eq!(parse_volume_probe(true, "\n", ""), VolumeProbe::Unlabelled);
        assert_eq!(
            parse_volume_probe(true, "<no value>\n", ""),
            VolumeProbe::Unlabelled
        );
        assert_eq!(
            parse_volume_probe(true, "abc\n", ""),
            VolumeProbe::Labelled("abc".into())
        );
    }

    #[test]
    fn unknown_probe_fails_closed() {
        assert!(matches!(
            plan_claim(
                &parse_volume_probe(false, "", "permission denied"),
                OWNER,
                None
            ),
            ClaimPlan::Refuse(_)
        ));
    }

    #[test]
    fn claim_dir_is_created_before_the_record() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        std::fs::create_dir_all(state.join("thegn")).unwrap();
        let mut runner = |_: &[String]| Some((true, "\n".into(), String::new()));
        claim_volumes(&spec(OWNER), &state, &mut runner).unwrap();
        let dir = state.join("thegn/devcontainer-volumes");
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }
}
