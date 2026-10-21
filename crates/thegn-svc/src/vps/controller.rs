//! Persistent identity for resources created by this thegn root.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;

use anyhow::Context;

use super::{CONTROLLER_KEY, HOST_KEY};

pub const CONTROLLER_ID_FILE: &str = "controller-id";

#[derive(Debug)]
pub enum ControllerIdError {
    Io(std::io::Error),
    Malformed,
    NotOwned,
    Linked,
}

impl fmt::Display for ControllerIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "controller id I/O failed: {error}"),
            Self::Malformed => f.write_str("controller id file is malformed"),
            Self::NotOwned => f.write_str("controller id file is not owned by the current user"),
            Self::Linked => f.write_str("controller id file is linked or not a regular file"),
        }
    }
}

impl std::error::Error for ControllerIdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Malformed | Self::NotOwned | Self::Linked => None,
        }
    }
}

impl From<std::io::Error> for ControllerIdError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn read_existing(path: &Path) -> Result<String, ControllerIdError> {
    let file = thegn_core::fsperm::open_regular_file_nofollow(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidInput {
            ControllerIdError::Linked
        } else {
            ControllerIdError::Io(error)
        }
    })?;
    let metadata = file.metadata()?;
    if !thegn_core::fsperm::owned_by_current_user(&metadata) {
        return Err(ControllerIdError::NotOwned);
    }
    thegn_core::fsperm::ensure_single_link_regular_file(&metadata)
        .map_err(|_| ControllerIdError::Linked)?;

    let mut bytes = Vec::with_capacity(33);
    file.take(128).read_to_end(&mut bytes)?;
    if bytes.len() == 128 {
        return Err(ControllerIdError::Malformed);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.len() != 32
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(ControllerIdError::Malformed);
    }
    String::from_utf8(bytes).map_err(|_| ControllerIdError::Malformed)
}

/// Load the controller id from `root`, creating it once with exclusive,
/// owner-only publication when no id exists yet.
pub fn controller_id_in(root: &Path) -> Result<String, ControllerIdError> {
    let path = root.join(CONTROLLER_ID_FILE);
    match read_existing(&path) {
        Ok(id) => Ok(id),
        Err(ControllerIdError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root)?;
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random)
                .map_err(|error| ControllerIdError::Io(std::io::Error::other(error.to_string())))?;
            let mut id = String::with_capacity(33);
            for byte in random {
                use fmt::Write as _;
                write!(&mut id, "{byte:02x}").map_err(|_| ControllerIdError::Malformed)?;
            }
            let mut file_bytes = id.as_bytes().to_vec();
            file_bytes.push(b'\n');
            let temporary = root.join(format!(".{CONTROLLER_ID_FILE}.tmp-{id}"));
            thegn_core::fsperm::write_owner_only_new(&temporary, &file_bytes)?;
            let publish = std::fs::hard_link(&temporary, &path);
            let cleanup = std::fs::remove_file(&temporary);
            match (publish, cleanup) {
                (Ok(()), Ok(())) => {
                    thegn_core::fsperm::sync_parent_dir(root)?;
                    Ok(id)
                }
                (Ok(()), Err(error)) => Err(ControllerIdError::Io(error)),
                (Err(publish_error), cleanup_result) => {
                    cleanup_result?;
                    if publish_error.kind() == std::io::ErrorKind::AlreadyExists {
                        read_existing(&path)
                    } else {
                        Err(ControllerIdError::Io(publish_error))
                    }
                }
            }
        }
        Err(error) => Err(error),
    }
}

/// Load the controller id for provider creation from this thegn root.
pub fn controller_id() -> anyhow::Result<String> {
    let root = thegn_core::util::thegn_dir();
    let path = root.join(CONTROLLER_ID_FILE);
    controller_id_in(&root).with_context(|| {
        format!(
            "vps: cannot load the controller id at {}; refusing to create",
            path.display()
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Ours,
    LegacyHost,
    Foreign,
    Unlabelled,
}

/// Classify provider labels against this controller and the legacy host scope.
pub fn classify_ownership(
    labels: &BTreeMap<String, String>,
    ours_ctl: &str,
    ours_host: &str,
) -> Ownership {
    if let Some(controller) = labels.get(CONTROLLER_KEY) {
        return if controller == ours_ctl {
            Ownership::Ours
        } else {
            Ownership::Foreign
        };
    }
    match labels.get(HOST_KEY) {
        Some(host) if host == ours_host => Ownership::LegacyHost,
        Some(_) => Ownership::Foreign,
        None => Ownership::Unlabelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_controller_id(value: &str) -> bool {
        value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    #[test]
    fn controller_id_is_created_once_and_stable() {
        let root = tempfile::tempdir().unwrap();
        let first = controller_id_in(root.path()).unwrap();
        let second = controller_id_in(root.path()).unwrap();
        assert!(is_controller_id(&first));
        assert_eq!(first, second);
        let mode = thegn_core::fsperm::mode_bits(&root.path().join(CONTROLLER_ID_FILE)).unwrap();
        assert!(mode.is_none() || mode == Some(0o600));
    }

    #[test]
    fn controller_id_differs_per_root() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first = controller_id_in(first_root.path()).unwrap();
        let second = controller_id_in(second_root.path()).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn controller_id_refuses_a_symlinked_file() {
        if cfg!(unix) {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            std::fs::write(&target, b"0123456789abcdef0123456789abcdef\n").unwrap();
            let id_path = root.path().join(CONTROLLER_ID_FILE);
            thegn_core::fsperm::create_symlink(&target, &id_path, false).unwrap();
            assert!(matches!(
                controller_id_in(root.path()),
                Err(ControllerIdError::Linked)
            ));
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"0123456789abcdef0123456789abcdef\n"
            );
        }
    }

    #[test]
    fn controller_id_refuses_malformed_contents_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        let id_path = root.path().join(CONTROLLER_ID_FILE);
        std::fs::write(&id_path, b"nothex").unwrap();
        assert!(matches!(
            controller_id_in(root.path()),
            Err(ControllerIdError::Malformed)
        ));
        assert_eq!(std::fs::read(id_path).unwrap(), b"nothex");
    }

    #[test]
    fn controller_id_refuses_a_hard_linked_file() {
        if cfg!(unix) {
            let root = tempfile::tempdir().unwrap();
            let id_path = root.path().join(CONTROLLER_ID_FILE);
            let other_path = root.path().join("other");
            std::fs::write(&id_path, b"0123456789abcdef0123456789abcdef\n").unwrap();
            std::fs::hard_link(&id_path, &other_path).unwrap();
            assert!(matches!(
                controller_id_in(root.path()),
                Err(ControllerIdError::Linked)
            ));
            assert_eq!(
                std::fs::read(other_path).unwrap(),
                b"0123456789abcdef0123456789abcdef\n"
            );
        }
    }

    #[test]
    fn classify_ownership_matrix() {
        let mut labels = BTreeMap::new();
        labels.insert(CONTROLLER_KEY.into(), "ours".into());
        labels.insert(HOST_KEY.into(), "different-host".into());
        assert_eq!(classify_ownership(&labels, "ours", "host"), Ownership::Ours);

        labels.insert(CONTROLLER_KEY.into(), "other".into());
        assert_eq!(
            classify_ownership(&labels, "ours", "host"),
            Ownership::Foreign
        );
        labels.insert(HOST_KEY.into(), "host".into());
        assert_eq!(
            classify_ownership(&labels, "ours", "host"),
            Ownership::Foreign
        );

        labels.remove(CONTROLLER_KEY);
        assert_eq!(
            classify_ownership(&labels, "ours", "host"),
            Ownership::LegacyHost
        );
        labels.insert(HOST_KEY.into(), "other-host".into());
        assert_eq!(
            classify_ownership(&labels, "ours", "host"),
            Ownership::Foreign
        );
        labels.clear();
        assert_eq!(
            classify_ownership(&labels, "ours", "host"),
            Ownership::Unlabelled
        );
    }
}
