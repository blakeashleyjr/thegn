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
use crate::placement::{Placement, SshPlacement};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

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
    /// Stable, domain-separated identity for ownership of this projection.
    /// This is distinct from the mountpoint component: the manifest binds the
    /// complete operation identity, including mode and the final local path.
    pub fn identity_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"thegn/projection-identity/v1");
        let mode = match self.mode {
            DataMode::InEnv => "in_env",
            DataMode::LocalExec => "local_exec",
            DataMode::Sshfs => "sshfs",
            DataMode::Sync => "sync",
        };
        hash_projection_field(&mut hasher, b"mode", mode.as_bytes());
        if let Placement::Ssh(ssh) = &self.placement {
            hash_projection_field(&mut hasher, b"host", ssh.host.as_bytes());
            hash_projection_field(&mut hasher, b"port", &ssh.port.to_be_bytes());
            hash_projection_option(&mut hasher, b"ssh_config", ssh.ssh_config.as_deref());
            hash_projection_option(&mut hasher, b"jump_host", ssh.jump_host.as_deref());
            hash_projection_option(&mut hasher, b"identity", ssh.identity.as_deref());
        } else {
            hash_projection_field(&mut hasher, b"placement", b"other");
        }
        hash_projection_field(&mut hasher, b"remote_dir", self.remote_dir.as_bytes());
        hash_projection_field(&mut hasher, b"mountpoint", self.mountpoint.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Pre-identity mountpoint candidates for ssh placements that are safe to target.
    ///
    /// The legacy slug collapses distinct `(host, path)` pairs, which is why
    /// current mountpoints include a digest. The result contains the old slug
    /// path and, for a non-default SSH placement, its former v1 digest path.
    /// That v1 path can coincide with another environment's live default-
    /// placement mount; a non-lazy `fusermount3 -u` may still unmount it when
    /// idle, so callers must not treat its success as proof that this
    /// placement's mount was removed. Do not make unmount lazy (`-uz`).
    pub fn legacy_mountpoints(&self) -> Vec<String> {
        let Placement::Ssh(ssh) = &self.placement else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        if let Some(slug_path) = legacy_mountpoint(&ssh.host, &self.remote_dir) {
            candidates.push(slug_path);
        }
        let v1_path = mountpoint_v1(&ssh.host, &self.remote_dir);
        if v1_path != self.mountpoint && !candidates.contains(&v1_path) {
            candidates.push(v1_path);
        }
        candidates
    }
}

/// A stable local mountpoint under `<thegn_dir>/mounts/<readable-prefix>--<digest>`.
/// The default SSH placement (`port == 22`, no jump host, identity, or ssh
/// config) retains the v1 host/path hash. Otherwise the digest also includes
/// those four tagged placement values. Values compare as exact UTF-8 bytes:
/// DNS aliases, path normalization, and equivalent SSH config semantics do not
/// coalesce. Transport kind, agent forwarding, and extra SSH args are not part
/// of this bounded placement identity.
pub fn mountpoint(placement: &SshPlacement, remote_path: &str) -> String {
    let component = projection_component(placement, remote_path);
    crate::util::thegn_dir()
        .join("mounts")
        .join(component)
        .to_string_lossy()
        .into_owned()
}

fn mountpoint_v1(host: &str, remote_path: &str) -> String {
    let component = projection_component_v1(host, remote_path);
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
    distinct_mountpoint(candidate, &mountpoint_v1(host, remote_path))
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
fn projection_component_v1(host: &str, remote_path: &str) -> String {
    const DOMAIN: &[u8] = b"thegn/projection-mountpoint/v1";
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    for part in [host.as_bytes(), remote_path.as_bytes()] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    projection_component_from_digest(host, remote_path, hasher)
}

fn projection_component(placement: &SshPlacement, remote_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"thegn/projection-mountpoint/v1");
    for part in [placement.host.as_bytes(), remote_path.as_bytes()] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    if placement.port != 22
        || placement.jump_host.is_some()
        || placement.identity.is_some()
        || placement.ssh_config.is_some()
    {
        hasher.update(b"thegn/projection-mountpoint/placement/v1");
        hash_projection_field(&mut hasher, b"port", &placement.port.to_be_bytes());
        hash_projection_option(&mut hasher, b"jump_host", placement.jump_host.as_deref());
        hash_projection_option(&mut hasher, b"identity", placement.identity.as_deref());
        hash_projection_option(&mut hasher, b"ssh_config", placement.ssh_config.as_deref());
    }
    projection_component_from_digest(&placement.host, remote_path, hasher)
}

fn hash_projection_field(hasher: &mut Sha256, tag: &[u8], value: &[u8]) {
    hasher.update((tag.len() as u64).to_be_bytes());
    hasher.update(tag);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn hash_projection_option(hasher: &mut Sha256, tag: &[u8], value: Option<&str>) {
    let mut encoded = Vec::new();
    if let Some(value) = value {
        encoded.push(1);
        encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
        encoded.extend_from_slice(value.as_bytes());
    } else {
        encoded.push(0);
    }
    hash_projection_field(hasher, tag, &encoded);
}

fn projection_component_from_digest(host: &str, remote_path: &str, hasher: Sha256) -> String {
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

/// Identifies legacy slug-only mount directory names. Current components
/// contain `--` before their digest, while `slugify` never emits that pair.
pub fn is_legacy_mount_component(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Mountpoints currently mounted from the `fuse.sshfs` filesystem, parsed from
/// `/proc/self/mountinfo`. Malformed lines are ignored.
pub fn sshfs_mountpoints_from_mountinfo(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let (before_separator, after_separator) = line.split_once(" - ")?;
            let fields = before_separator.split_whitespace().collect::<Vec<_>>();
            let filesystem_fields = after_separator.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 6 || filesystem_fields.len() < 3 {
                return None;
            }
            (filesystem_fields[0] == "fuse.sshfs").then(|| unescape_mountinfo(fields[4]))
        })
        .collect()
}

fn unescape_mountinfo(mountpoint: &str) -> String {
    mountpoint
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Action to take for a legacy mount directory, given mountinfo knowledge and
/// whether lstat found a real directory and that directory is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyReap {
    Unmount,
    RemoveOnly,
    Skip,
}

pub fn legacy_reap_action(mounted: Option<bool>, is_real_dir: bool, is_empty: bool) -> LegacyReap {
    match mounted {
        Some(true) => LegacyReap::Unmount,
        Some(false) if is_real_dir && is_empty => LegacyReap::RemoveOnly,
        Some(false) => LegacyReap::Skip,
        None if is_real_dir => LegacyReap::Unmount,
        None => LegacyReap::Skip,
    }
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
            let mountpoint = mountpoint(s, &remote_dir);
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

    fn ssh_placement() -> SshPlacement {
        SshPlacement::plain("dev-box".into(), 22, false, TransportKind::Ssh)
    }

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
        assert_eq!(spec.mountpoint, mountpoint(&ssh_placement(), "/srv/work"));
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
        assert_ne!(expected, mountpoint(&ssh_placement(), "/srv/work"));
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
    fn sshfs_spec_exposes_legacy_mountpoints() {
        let env = ssh_env(DataMode::Sshfs, "/srv/work");
        let spec = for_environment(&env).expect("sshfs spec");
        assert_eq!(spec.legacy_mountpoints().len(), 1);

        let mut local_spec = spec;
        local_spec.placement = Placement::Local;
        assert!(local_spec.legacy_mountpoints().is_empty());
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
            .map(|(host, path)| {
                projection_component(
                    &SshPlacement::plain((*host).into(), 22, false, TransportKind::Ssh),
                    path,
                )
            })
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
        let component = projection_component(
            &SshPlacement::plain("x".repeat(100_000), 22, false, TransportKind::Ssh),
            &"/".repeat(100_000),
        );
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

        let punctuation_only = projection_component(
            &SshPlacement::plain("///".into(), 22, false, TransportKind::Ssh),
            "\\\\",
        );
        assert!(punctuation_only.starts_with("projection--"));
        assert!(!punctuation_only.is_empty());
        let path = mountpoint(
            &SshPlacement::plain("///".into(), 22, false, TransportKind::Ssh),
            "\\\\",
        );
        assert_eq!(
            std::path::Path::new(&path).file_name().unwrap(),
            std::ffi::OsStr::new(&punctuation_only)
        );
    }

    #[test]
    fn default_placement_keeps_v1_mountpoint_and_nondefault_knobs_separate() {
        let default = ssh_placement();
        assert_eq!(
            projection_component(&default, "/srv/work"),
            "dev-box-srv-work--eac76ad6c9ad2796ac78198144530994"
        );
        assert_eq!(
            mountpoint(&default, "/srv/work"),
            mountpoint_v1("dev-box", "/srv/work")
        );
        let mut port_2222 = SshPlacement::plain("dev-box".into(), 2222, false, TransportKind::Ssh);
        let mut port_2223 = port_2222.clone();
        port_2223.port = 2223;
        assert_ne!(
            projection_component(&port_2222, "/srv/work"),
            projection_component(&port_2223, "/srv/work")
        );
        assert_ne!(
            projection_component(&default, "/srv/work"),
            projection_component(&port_2222, "/srv/work")
        );

        port_2222.jump_host = Some("bastion".into());
        let jump = projection_component(&port_2222, "/srv/work");
        port_2222.jump_host = None;
        port_2222.identity = Some("/keys/id".into());
        let identity = projection_component(&port_2222, "/srv/work");
        port_2222.identity = None;
        port_2222.ssh_config = Some("/cfg/ssh".into());
        let config = projection_component(&port_2222, "/srv/work");
        assert_ne!(jump, identity);
        assert_ne!(identity, config);
    }

    #[test]
    fn legacy_mountpoints_include_v1_for_nondefault_placement() {
        let mut env = ssh_env(DataMode::Sshfs, "/srv/work");
        let Placement::Ssh(ssh) = &mut env.placement else {
            unreachable!()
        };
        ssh.port = 2222;
        let spec = for_environment(&env).expect("sshfs spec");
        assert_eq!(spec.legacy_mountpoints().len(), 2);
        assert!(
            spec.legacy_mountpoints()
                .contains(&mountpoint(&ssh_placement(), "/srv/work"))
        );
    }

    #[test]
    fn legacy_mount_component_classifier_matches_slug_domain() {
        for (name, expected) in [
            ("dev-box-srv-work", true),
            ("a", true),
            ("", false),
            ("--", false),
            ("name--0123", false),
            ("Upper", false),
            ("has_under", false),
            ("é", false),
        ] {
            assert_eq!(is_legacy_mount_component(name), expected, "{name:?}");
        }
    }

    #[test]
    fn mountinfo_parser_keeps_only_sshfs_and_unescapes_mountpoints() {
        let mountinfo = concat!(
            "31 22 0:28 / /m/a\\040b rw,nosuid - fuse.sshfs host:/a rw\n",
            "32 22 8:1 / /mnt/disk rw - ext4 /dev/sda1 rw\n",
            "33 22 0:45 / /mnt/cloud rw - fuse.rclone remote: rw\n",
            "34 22 0:46 / truncated rw\n",
            "35 22 0:47 / /mnt/no-separator rw\n",
            "36 22 0:48 / - fuse.sshfs\n",
        );

        assert_eq!(
            sshfs_mountpoints_from_mountinfo(mountinfo),
            BTreeSet::from(["/m/a b".to_string()])
        );
    }

    #[test]
    fn legacy_reap_action_covers_known_unknown_and_directory_states() {
        for (mounted, is_real_dir, is_empty, expected) in [
            (Some(true), false, false, LegacyReap::Unmount),
            (Some(false), true, true, LegacyReap::RemoveOnly),
            (Some(false), true, false, LegacyReap::Skip),
            (Some(false), false, true, LegacyReap::Skip),
            (None, true, false, LegacyReap::Unmount),
            (None, false, true, LegacyReap::Skip),
        ] {
            assert_eq!(legacy_reap_action(mounted, is_real_dir, is_empty), expected);
        }
    }

    #[test]
    fn projection_identity_digest_is_stable_and_covers_identity_fields() {
        let base = ProjectionSpec {
            mode: DataMode::Sync,
            placement: Placement::Ssh(ssh_placement()),
            remote_dir: "/srv/work".into(),
            mountpoint: "/tmp/mounts/work".into(),
        };
        let digest = base.identity_digest();
        assert_eq!(digest, base.identity_digest());

        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.mode = DataMode::Sshfs;
        variants.push(changed);
        let mut changed = base.clone();
        changed.remote_dir.push_str("-other");
        variants.push(changed);
        let mut changed = base.clone();
        changed.mountpoint.push_str("-other");
        variants.push(changed);
        let mut changed = base.clone();
        changed.placement = Placement::Local;
        variants.push(changed);
        let ssh_changes: [fn(&mut SshPlacement); 5] = [
            |ssh: &mut SshPlacement| ssh.host.push_str("-other"),
            |ssh: &mut SshPlacement| ssh.port += 1,
            |ssh: &mut SshPlacement| ssh.ssh_config = Some("/cfg/ssh".into()),
            |ssh: &mut SshPlacement| ssh.jump_host = Some("bastion".into()),
            |ssh: &mut SshPlacement| ssh.identity = Some("/keys/id".into()),
        ];
        for change_ssh in ssh_changes {
            let mut changed = base.clone();
            let Placement::Ssh(ssh) = &mut changed.placement else {
                unreachable!()
            };
            change_ssh(ssh);
            variants.push(changed);
        }
        assert!(
            variants
                .iter()
                .all(|variant| variant.identity_digest() != digest)
        );
    }
}
