//! Owner manifest for local SSHFS and sync projection mountpoints.

use std::io::Read;
use std::path::{Path, PathBuf};

use thegn_core::fsperm;
use thegn_core::projection::ProjectionSpec;

const MANIFEST_LIMIT: u64 = 4 * 1024;

#[derive(Debug)]
pub enum ProjectionOwnershipError {
    Missing(PathBuf),
    Malformed(String),
    Unsafe(String),
    Mismatch(PathBuf),
    Occupied(PathBuf),
}

impl std::fmt::Display for ProjectionOwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(path) => write!(
                f,
                "projection manifest {} is missing; move {} aside and mount again",
                path.display(),
                path.display()
            ),
            Self::Malformed(message) => write!(
                f,
                "malformed projection manifest: {message}; move the manifest aside and mount again"
            ),
            Self::Unsafe(message) => write!(
                f,
                "unsafe projection ownership state: {message}; inspect or move the manifest aside"
            ),
            Self::Mismatch(path) => write!(
                f,
                "projection manifest {} belongs to another projection; move it aside",
                path.display()
            ),
            Self::Occupied(path) => write!(
                f,
                "unowned projection mountpoint {} is occupied; move {} aside",
                path.display(),
                path.display()
            ),
        }
    }
}

impl std::error::Error for ProjectionOwnershipError {}

pub(super) fn manifest_path(spec: &ProjectionSpec) -> PathBuf {
    PathBuf::from(format!("{}.thegn-projection", spec.mountpoint))
}

pub(super) fn claim_or_verify(spec: &ProjectionSpec) -> Result<(), ProjectionOwnershipError> {
    let mountpoint = Path::new(&spec.mountpoint);
    let mounts_dir = mountpoint.parent().ok_or_else(|| {
        ProjectionOwnershipError::Unsafe(format!(
            "mountpoint {} has no parent directory",
            mountpoint.display()
        ))
    })?;
    fsperm::ensure_private_dir(mounts_dir).map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!("{}: {error}", mounts_dir.display()))
    })?;

    let manifest = manifest_path(spec);
    match std::fs::symlink_metadata(&manifest) {
        Ok(_) => return verify(spec),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ProjectionOwnershipError::Unsafe(format!(
                "{}: {error}",
                manifest.display()
            )));
        }
    }

    match std::fs::symlink_metadata(mountpoint) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || !fsperm::owned_by_current_user(&metadata)
            {
                return Err(ProjectionOwnershipError::Occupied(mountpoint.to_path_buf()));
            }
            let mut entries = std::fs::read_dir(mountpoint).map_err(|error| {
                ProjectionOwnershipError::Unsafe(format!("{}: {error}", mountpoint.display()))
            })?;
            if entries.next().is_some() {
                return Err(ProjectionOwnershipError::Occupied(mountpoint.to_path_buf()));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ProjectionOwnershipError::Unsafe(format!(
                "{}: {error}",
                mountpoint.display()
            )));
        }
    }

    fsperm::ensure_private_dir(mountpoint).map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!("{}: {error}", mountpoint.display()))
    })?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!(
            "{}: could not generate manifest generation: {error}",
            manifest.display()
        ))
    })?;
    let generation = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let contents = format!(
        "thegn-projection v1\nidentity={}\ngeneration={generation}\n",
        spec.identity_digest()
    );
    fsperm::write_owner_only_atomic(&manifest, contents.as_bytes()).map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!("{}: {error}", manifest.display()))
    })
}

pub(super) fn verify(spec: &ProjectionSpec) -> Result<(), ProjectionOwnershipError> {
    let mountpoint = Path::new(&spec.mountpoint);
    let manifest = manifest_path(spec);
    let metadata = match std::fs::symlink_metadata(&manifest) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProjectionOwnershipError::Missing(manifest));
        }
        Err(error) => {
            return Err(ProjectionOwnershipError::Unsafe(format!(
                "{}: {error}",
                manifest.display()
            )));
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || !fsperm::owned_by_current_user(&metadata)
    {
        return Err(ProjectionOwnershipError::Unsafe(format!(
            "{} is not an owner-owned regular file",
            manifest.display()
        )));
    }
    fsperm::ensure_single_link_regular_file(&metadata).map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!("{}: {error}", manifest.display()))
    })?;
    if fsperm::mode_bits(&manifest)
        .map_err(|error| {
            ProjectionOwnershipError::Unsafe(format!("{}: {error}", manifest.display()))
        })?
        .is_some_and(|mode| mode & 0o077 != 0)
    {
        return Err(ProjectionOwnershipError::Unsafe(format!(
            "{} has group or other permissions",
            manifest.display()
        )));
    }
    if metadata.len() > MANIFEST_LIMIT {
        return Err(ProjectionOwnershipError::Malformed(format!(
            "{} exceeds {MANIFEST_LIMIT} bytes",
            manifest.display()
        )));
    }

    let file = fsperm::open_regular_file_nofollow(&manifest).map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!("{}: {error}", manifest.display()))
    })?;
    let opened_metadata = file.metadata().map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!("{}: {error}", manifest.display()))
    })?;
    if !fsperm::owned_by_current_user(&opened_metadata)
        || fsperm::ensure_single_link_regular_file(&opened_metadata).is_err()
        || !opened_metadata.is_file()
    {
        return Err(ProjectionOwnershipError::Unsafe(format!(
            "{} changed while opening",
            manifest.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ProjectionOwnershipError::Unsafe(format!("{}: {error}", manifest.display()))
        })?;
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(ProjectionOwnershipError::Malformed(format!(
            "{} exceeds {MANIFEST_LIMIT} bytes",
            manifest.display()
        )));
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        ProjectionOwnershipError::Malformed(format!("{}: {error}", manifest.display()))
    })?;
    let mut lines = text.split_terminator('\n');
    let header = lines.next();
    let identity = lines.next().and_then(|line| line.strip_prefix("identity="));
    let generation = lines
        .next()
        .and_then(|line| line.strip_prefix("generation="));
    if !text.ends_with('\n')
        || header != Some("thegn-projection v1")
        || lines.next().is_some()
        || !identity.is_some_and(is_digest)
        || !generation.is_some_and(is_nonce)
    {
        return Err(ProjectionOwnershipError::Malformed(format!(
            "{} has an invalid v1 record",
            manifest.display()
        )));
    }
    if identity != Some(spec.identity_digest().as_str()) {
        return Err(ProjectionOwnershipError::Mismatch(manifest));
    }
    let mountpoint_metadata = std::fs::symlink_metadata(mountpoint).map_err(|error| {
        ProjectionOwnershipError::Unsafe(format!("{}: {error}", mountpoint.display()))
    })?;
    if mountpoint_metadata.file_type().is_symlink() || !mountpoint_metadata.is_dir() {
        return Err(ProjectionOwnershipError::Unsafe(format!(
            "{} is not a real directory",
            mountpoint.display()
        )));
    }
    Ok(())
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{ProjectionBackend, for_data_mode};
    use thegn_core::config::DataMode;
    use thegn_core::placement::{Placement, SshPlacement, TransportKind};

    fn fixture(mode: DataMode) -> (tempfile::TempDir, ProjectionSpec) {
        let root = tempfile::tempdir().expect("private fixture directory");
        fsperm::ensure_private_dir(root.path()).expect("restrict fixture directory");
        let mountpoint = root.path().join("mounts").join("dev-box--projection");
        let spec = ProjectionSpec {
            mode,
            placement: Placement::Ssh(SshPlacement::plain(
                "dev-box".into(),
                22,
                false,
                TransportKind::Ssh,
            )),
            remote_dir: "/srv/work".into(),
            mountpoint: mountpoint.to_string_lossy().into_owned(),
        };
        (root, spec)
    }

    #[test]
    fn fresh_mountpoint_is_claimed_then_verified() {
        let (_root, spec) = fixture(DataMode::Sync);
        claim_or_verify(&spec).expect("claim fresh projection");
        verify(&spec).expect("verify claimed projection");
        let manifest = manifest_path(&spec);
        let mode = fsperm::mode_bits(&manifest).expect("read manifest mode");
        assert!(mode.is_none_or(|bits| bits & 0o777 == 0o600));
        let text = std::fs::read_to_string(manifest).expect("read manifest");
        assert!(text.starts_with("thegn-projection v1\nidentity="));
        assert!(text.contains("\ngeneration="));
    }

    #[test]
    fn non_empty_unowned_directory_is_refused() {
        let (_root, spec) = fixture(DataMode::Sync);
        let mountpoint = Path::new(&spec.mountpoint);
        std::fs::create_dir_all(mountpoint).expect("create occupied mountpoint");
        let file = mountpoint.join("working-copy");
        std::fs::write(&file, "preserve me").expect("write unowned contents");

        assert!(matches!(
            claim_or_verify(&spec),
            Err(ProjectionOwnershipError::Occupied(_))
        ));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "preserve me");
    }

    #[test]
    fn missing_manifest_refuses_push_and_unmount() {
        let (_root, spec) = fixture(DataMode::Sync);
        assert!(matches!(
            verify(&spec),
            Err(ProjectionOwnershipError::Missing(_))
        ));
    }

    #[test]
    fn stale_manifest_for_another_identity_refuses() {
        let (_root, mut spec) = fixture(DataMode::Sync);
        claim_or_verify(&spec).expect("claim first identity");
        spec.remote_dir.push_str("-other");
        assert!(matches!(
            verify(&spec),
            Err(ProjectionOwnershipError::Mismatch(_))
        ));
    }

    #[test]
    fn malformed_manifest_refuses_reuse() {
        let (_root, spec) = fixture(DataMode::Sync);
        claim_or_verify(&spec).expect("claim projection");
        std::fs::write(manifest_path(&spec), "not a manifest\n").expect("corrupt manifest");
        assert!(matches!(
            verify(&spec),
            Err(ProjectionOwnershipError::Malformed(_))
        ));
    }

    #[test]
    fn symlinked_or_group_readable_manifest_refuses() {
        let (_root, spec) = fixture(DataMode::Sync);
        claim_or_verify(&spec).expect("claim projection");
        let manifest = manifest_path(&spec);
        std::fs::remove_file(&manifest).expect("remove manifest for symlink fixture");
        let target = Path::new(&spec.mountpoint).parent().unwrap().join("target");
        std::fs::write(&target, "thegn-projection v1\n").expect("write symlink target");
        if fsperm::create_symlink(&target, &manifest, false).is_err() {
            return;
        }
        assert!(matches!(
            verify(&spec),
            Err(ProjectionOwnershipError::Unsafe(_))
        ));
    }

    #[test]
    fn sshfs_mount_refuses_occupied_mountpoint_before_subprocess() {
        let (_root, spec) = fixture(DataMode::Sshfs);
        let mountpoint = Path::new(&spec.mountpoint);
        std::fs::create_dir_all(mountpoint).expect("create occupied mountpoint");
        std::fs::write(mountpoint.join("keep"), "data").expect("write occupant");
        let error = for_data_mode(&spec).mount(&spec).unwrap_err();
        assert!(error.to_string().contains("occupied"));
    }

    #[test]
    fn sync_mount_refuses_occupied_mountpoint_before_pull() {
        let (_root, spec) = fixture(DataMode::Sync);
        let mountpoint = Path::new(&spec.mountpoint);
        std::fs::create_dir_all(mountpoint).expect("create occupied mountpoint");
        std::fs::write(mountpoint.join("keep"), "data").expect("write occupant");
        let error = for_data_mode(&spec).mount(&spec).unwrap_err();
        assert!(error.to_string().contains("occupied"));
    }

    #[test]
    fn sync_refresh_refuses_missing_manifest_before_push() {
        let (_root, spec) = fixture(DataMode::Sync);
        let error = for_data_mode(&spec).refresh(&spec).unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn sshfs_unmount_refuses_missing_manifest_before_fusermount() {
        let (_root, spec) = fixture(DataMode::Sshfs);
        let error = for_data_mode(&spec).unmount(&spec).unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn sync_unmount_refuses_missing_manifest_before_push() {
        let (_root, spec) = fixture(DataMode::Sync);
        let error = for_data_mode(&spec).unmount(&spec).unwrap_err();
        assert!(error.to_string().contains("missing"));
    }
}
