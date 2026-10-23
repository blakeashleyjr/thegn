use std::path::{Path, PathBuf};

pub(crate) fn socket_dir(socket: &Path) -> Option<PathBuf> {
    socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

pub(crate) fn run_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("run")
}

pub(crate) fn publish(socket: &Path, state_dir: &Path) {
    if let Some(dir) = socket_dir(socket) {
        thegn_core::sandbox_control_guard::publish_control_socket_dir(dir);
    }
    thegn_core::sandbox_control_guard::publish_protected_state_dir(run_dir(state_dir));
}

pub(crate) fn publish_for_socket(socket: &Path) {
    publish(socket, &thegn_core::util::xdg_state_home().join("thegn"));
}

pub(crate) fn prepare_run_dir(state_dir: &Path) -> std::io::Result<()> {
    let _run_dir = thegn_core::fsperm::ensure_private_dir_beneath(state_dir, Path::new("run"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{prepare_run_dir, run_dir, socket_dir};
    use std::path::{Path, PathBuf};

    #[test]
    fn socket_dir_is_the_non_empty_parent() {
        assert_eq!(
            socket_dir(Path::new("/tmp/thegn-42-k-0123456789abcdef/daemon.sock")),
            Some(PathBuf::from("/tmp/thegn-42-k-0123456789abcdef"))
        );
        assert_eq!(socket_dir(Path::new("daemon.sock")), None);
    }

    #[test]
    fn run_dir_is_the_state_dirs_run_child() {
        assert_eq!(
            run_dir(Path::new("/state/thegn")),
            PathBuf::from("/state/thegn/run")
        );
    }

    #[test]
    fn prepare_run_dir_creates_the_directory() {
        let state = tempfile::tempdir_in("/tmp").unwrap();
        prepare_run_dir(state.path()).unwrap();
        assert!(state.path().join("run").is_dir());
        prepare_run_dir(state.path()).unwrap();
    }
}
