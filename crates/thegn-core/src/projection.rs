//! Worktree **projection** — the pure plan for the `data` axis of a named
//! `Environment`: how the worktree's files are made
//! available where the env runs.
//!
//! This mirrors the VPN split: the testable plan lives here in core; the *action*
//! (mount / sync / unmount, which shells out to `sshfs`/`rsync`/a provider file
//! API) lives in `thegn-svc::projection`. A [`ProjectionSpec`] is assembled
//! from a resolved [`Environment`] by [`for_environment`]; the svc layer dispatches
//! on its [`mode`](ProjectionSpec::mode) to the right backend.
//!
//! For the default `in_env` data mode (and `local_exec`) there is nothing to
//! project — the placement already lands the process in the worktree — so
//! [`for_environment`] returns `None` and the lifecycle is a no-op. Only the
//! mounting/syncing modes (`sshfs`, and `sync` once it lands) yield a spec.

use crate::config::DataMode;
use crate::env::Environment;
use crate::placement::Placement;
use sha2::{Digest, Sha256};

/// A resolved plan for projecting one worktree into its execution environment.
/// Pure data; the bring-up/teardown lives in `thegn-svc::projection`.
#[derive(Debug, Clone)]
pub struct ProjectionSpec {
    /// The data mode this plan realizes (`Sshfs`, later `Sync`).
    pub mode: DataMode,
    /// The placement — carries the ssh knobs (host/port/identity/jump) the
    /// mounting backends need.
    pub placement: Placement,
    /// The remote tree to project (the sshfs source / sync peer).
    pub remote_dir: String,
    /// A stable local mountpoint under `<thegn_dir>/mounts` (the sshfs/sync
    /// target, and the cwd a pane should use once projected).
    pub mountpoint: String,
}

impl ProjectionSpec {
    /// The pre-digest mountpoint for ssh placements, if it is safe to target.
    ///
    /// The legacy slug collapses distinct `(host, path)` pairs, which is why
    /// current mountpoints include a digest. Teardown may therefore target a
    /// colliding pre-upgrade mount belonging to another env. A non-lazy
    /// `fusermount3 -u` refuses a busy mount, so this is safe; do not make it
    /// lazy (`-uz`).
    pub fn legacy_mountpoint(&self) -> Option<String> {
        let Placement::Ssh(ssh) = &self.placement else {
            return None;
        };
        legacy_mountpoint(&ssh.host, &self.remote_dir)
    }
}

/// A stable local mountpoint for the exact UTF-8 bytes of a host and remote
/// path, under `<thegn_dir>/mounts/<readable-prefix>--<digest>`. The digest is
/// versioned, domain-separated, and length-delimited so separators and
/// concatenation boundaries cannot alias. Only byte-identical pairs coalesce;
/// host aliases and path normalization are not inferred here.
pub fn mountpoint(host: &str, remote_path: &str) -> String {
    let component = projection_component(host, remote_path);
    crate::util::thegn_dir()
        .join("mounts")
        .join(component)
        .to_string_lossy()
        .into_owned()
}

/// The mountpoint formula used before projection mountpoints gained a digest.
/// Returns `None` when the old slug would target the mounts directory itself or
/// when it happens to equal the current mountpoint.
pub fn legacy_mountpoint(host: &str, remote_path: &str) -> Option<String> {
    let component = crate::util::slugify(&format!("{host}-{remote_path}"));
    if component.is_empty() {
        return None;
    }
    let candidate = crate::util::thegn_dir().join("mounts").join(component);
    distinct_mountpoint(candidate, &mountpoint(host, remote_path))
}

fn distinct_mountpoint(candidate: std::path::PathBuf, current: &str) -> Option<String> {
    if candidate == std::path::Path::new(current) {
        None
    } else {
        Some(candidate.to_string_lossy().into_owned())
    }
}

const PROJECTION_PREFIX_MAX_BYTES: usize = 40;
const PROJECTION_DIGEST_HEX_BYTES: usize = 32; // 128 bits

/// Build a filesystem-safe, bounded path component. The slug is display-only;
/// the SHA-256 suffix is computed from the exact pair of input byte strings.
fn projection_component(host: &str, remote_path: &str) -> String {
    const DOMAIN: &[u8] = b"thegn/projection-mountpoint/v1";
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    for part in [host.as_bytes(), remote_path.as_bytes()] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let digest_hex: String = digest[..PROJECTION_DIGEST_HEX_BYTES / 2]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    let mut prefix = String::with_capacity(PROJECTION_PREFIX_MAX_BYTES);
    let mut separator_pending = false;
    for byte in host
        .bytes()
        .chain(std::iter::once(b'-'))
        .chain(remote_path.bytes())
    {
        if byte.is_ascii_alphanumeric() {
            if separator_pending && !prefix.is_empty() {
                prefix.push('-');
            }
            separator_pending = false;
            prefix.push(byte.to_ascii_lowercase() as char);
            if prefix.len() == PROJECTION_PREFIX_MAX_BYTES {
                break;
            }
        } else {
            separator_pending = true;
        }
    }
    if prefix.is_empty() {
        prefix.push_str("projection");
    }
    format!("{prefix}--{digest_hex}")
}

/// Build the projection plan for a resolved environment, or `None` when the data
/// mode needs no projection (`in_env`/`local_exec`) or the inputs are incomplete
/// (e.g. `sshfs` without an ssh placement or a `remote_dir`).
pub fn for_environment(env: &Environment) -> Option<ProjectionSpec> {
    match env.data {
        // Both project a remote tree to a local mountpoint over ssh.
        DataMode::Sshfs | DataMode::Sync => {
            let Placement::Ssh(s) = &env.placement else {
                return None;
            };
            let remote_dir = env.sandbox.remote.remote_dir.trim().to_string();
            if remote_dir.is_empty() {
                return None;
            }
            let mountpoint = mountpoint(&s.host, &remote_dir);
            Some(ProjectionSpec {
                mode: env.data,
                placement: env.placement.clone(),
                remote_dir,
                mountpoint,
            })
        }
        DataMode::InEnv | DataMode::LocalExec => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;
    use crate::placement::{SshPlacement, TransportKind};

    fn ssh_env(data: DataMode, remote_dir: &str) -> Environment {
        let mut sandbox = SandboxConfig::default();
        sandbox.remote.remote_dir = remote_dir.to_string();
        Environment {
            name: "remote".into(),
            placement: Placement::Ssh(SshPlacement::plain(
                "dev-box".into(),
                22,
                false,
                TransportKind::Ssh,
            )),
            sandbox,
            data,
            unresolved_selection: false,
        }
    }

    #[test]
    fn in_env_needs_no_projection() {
        let env = ssh_env(DataMode::InEnv, "/srv/work");
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn local_exec_needs_no_projection() {
        let env = ssh_env(DataMode::LocalExec, "/srv/work");
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn sshfs_without_remote_dir_is_none() {
        let env = ssh_env(DataMode::Sshfs, "   ");
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn sshfs_without_ssh_placement_is_none() {
        let mut env = ssh_env(DataMode::Sshfs, "/srv/work");
        env.placement = Placement::Local;
        assert!(for_environment(&env).is_none());
    }

    #[test]
    fn sshfs_yields_spec_with_stable_mountpoint() {
        let env = ssh_env(DataMode::Sshfs, "/srv/work");
        let spec = for_environment(&env).expect("sshfs spec");
        assert_eq!(spec.mode, DataMode::Sshfs);
        assert_eq!(spec.remote_dir, "/srv/work");
        // Deterministic: recomputing from the same inputs matches.
        assert_eq!(spec.mountpoint, mountpoint("dev-box", "/srv/work"));
        assert!(spec.mountpoint.contains("mounts"));
    }

    #[test]
    fn legacy_mountpoint_reproduces_old_slug_formula() {
        let expected = crate::util::thegn_dir()
            .join("mounts")
            .join("dev-box-srv-work")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            legacy_mountpoint("dev-box", "/srv/work"),
            Some(expected.clone())
        );
        assert_ne!(expected, mountpoint("dev-box", "/srv/work"));
    }

    #[test]
    fn legacy_mountpoint_rejects_empty_slug_and_current_path() {
        assert_eq!(legacy_mountpoint("///", "\\\\"), None);
        let current = std::path::PathBuf::from("/tmp/mounts/current");
        assert_eq!(
            distinct_mountpoint(current.clone(), current.to_str().unwrap()),
            None
        );
    }

    #[test]
    fn sshfs_spec_exposes_legacy_mountpoint() {
        let env = ssh_env(DataMode::Sshfs, "/srv/work");
        let spec = for_environment(&env).expect("sshfs spec");
        assert!(spec.legacy_mountpoint().is_some());

        let mut local_spec = spec;
        local_spec.placement = Placement::Local;
        assert_eq!(local_spec.legacy_mountpoint(), None);
    }

    #[test]
    fn projection_mountpoint_digest_separates_old_slug_collisions() {
        let pairs = [
            ("a_b", "c"),
            ("a-b", "c"),
            ("A-B", "c"),
            ("a", "b-c"),
            ("a-", "b-c"),
            ("a", "-b-c"),
            ("é", "/tree"),
            ("e\u{301}", "/tree"),
        ];
        let components: Vec<_> = pairs
            .iter()
            .map(|(host, path)| projection_component(host, path))
            .collect();
        for (index, component) in components.iter().enumerate() {
            assert!(
                components[index + 1..]
                    .iter()
                    .all(|other| other != component)
            );
        }
    }

    #[test]
    fn projection_mountpoint_component_is_bounded_safe_and_never_empty() {
        let component = projection_component(&"x".repeat(100_000), &"/".repeat(100_000));
        assert!(component.len() <= PROJECTION_PREFIX_MAX_BYTES + 2 + PROJECTION_DIGEST_HEX_BYTES);
        assert!(!component.is_empty());
        assert!(
            component
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        assert!(!component.contains('/'));
        assert_eq!(
            component.rsplit_once("--").unwrap().1.len(),
            PROJECTION_DIGEST_HEX_BYTES
        );

        let punctuation_only = projection_component("///", "\\\\");
        assert!(punctuation_only.starts_with("projection--"));
        assert!(!punctuation_only.is_empty());
        let path = mountpoint("///", "\\\\");
        assert_eq!(
            std::path::Path::new(&path).file_name().unwrap(),
            std::ffi::OsStr::new(&punctuation_only)
        );
    }
}
