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
    publish_socket_to(&mut GuardSink, socket, state_dir);
}

trait PublishSink {
    fn control_socket_dir(&mut self, dir: PathBuf);
    fn protected_state_dir(&mut self, dir: PathBuf);
}

struct GuardSink;

impl PublishSink for GuardSink {
    fn control_socket_dir(&mut self, dir: PathBuf) {
        thegn_core::sandbox_control_guard::publish_control_socket_dir(dir);
    }

    fn protected_state_dir(&mut self, dir: PathBuf) {
        thegn_core::sandbox_control_guard::publish_protected_state_dir(dir);
    }
}

fn publish_to(sink: &mut dyn PublishSink, socket: &Path, state_dir: &Path) {
    if let Some(dir) = socket_dir(socket) {
        sink.control_socket_dir(dir);
    }
    sink.protected_state_dir(run_dir(state_dir));
}

fn publish_socket_to(sink: &mut dyn PublishSink, socket: &Path, state_dir: &Path) {
    publish_socket_candidates_to(
        sink,
        socket.to_path_buf(),
        super::private_tmp_socket_path(state_dir, super::effective_uid()),
        state_dir,
    );
}

fn publish_socket_candidates_to(
    sink: &mut dyn PublishSink,
    socket: PathBuf,
    predictable: PathBuf,
    state_dir: &Path,
) {
    let candidates = control_socket_candidates_from(socket, predictable);
    for candidate in candidates {
        publish_to(sink, &candidate, state_dir);
    }
}

pub(crate) fn publish_for_socket(socket: &Path) {
    let state_dir = thegn_core::util::xdg_state_home().join("thegn");
    publish_for_socket_at(socket, &state_dir, &mut GuardSink);
}

fn publish_for_socket_at(socket: &Path, state_dir: &Path, sink: &mut dyn PublishSink) {
    if let Err(error) = prepare_run_dir(state_dir) {
        tracing::warn!(target: "thegn::sandbox", path = %state_dir.display(), %error, "could not create the private control locator directory; writable-root panes may be refused");
    }
    publish_socket_to(sink, socket, state_dir);
}

/// The control sockets this process must publish for `dcfg`: the resolved endpoint and, when it is a relocated
/// sibling of the predictable `/tmp` fallback, the predictable one too (a pane must not reach either).
fn control_socket_candidates(dcfg: &thegn_core::config::DaemonConfig) -> Vec<PathBuf> {
    let state_dir = thegn_core::util::xdg_state_home().join("thegn");
    control_socket_candidates_from(
        super::socket_path(dcfg),
        super::private_tmp_socket_path(&state_dir, super::effective_uid()),
    )
}

fn control_socket_candidates_from(resolved: PathBuf, predictable: PathBuf) -> Vec<PathBuf> {
    let mut sockets = vec![resolved.clone()];
    if resolved != predictable
        && let (Some(resolved_parent), Some(predictable_parent)) =
            (resolved.parent(), predictable.parent())
        && resolved_parent.parent() == predictable_parent.parent()
        && let (Some(resolved_name), Some(predictable_name)) =
            (resolved_parent.file_name(), predictable_parent.file_name())
    {
        let mut relocated_prefix = predictable_name.as_encoded_bytes().to_vec();
        relocated_prefix.push(b'-');
        if resolved_name
            .as_encoded_bytes()
            .starts_with(&relocated_prefix)
        {
            sockets.push(predictable);
        }
    }
    sockets
}

pub(crate) fn publish_for_config(dcfg: &thegn_core::config::DaemonConfig) {
    let state_dir = thegn_core::util::xdg_state_home().join("thegn");
    let sockets = control_socket_candidates(dcfg);
    publish_candidates_at(&sockets, &state_dir, &mut GuardSink);
}

fn publish_candidates_at(sockets: &[PathBuf], state_dir: &Path, sink: &mut dyn PublishSink) {
    if let Err(error) = prepare_run_dir(state_dir) {
        tracing::warn!(target: "thegn::sandbox", path = %state_dir.display(), %error, "could not create the private control locator directory; writable-root panes may be refused");
    }
    for socket in sockets {
        publish_to(sink, socket, state_dir);
    }
}

pub(crate) fn prepare_run_dir(state_dir: &Path) -> std::io::Result<()> {
    let run_dir = run_dir(state_dir);
    let mut anchor = state_dir;
    while !anchor.is_dir() {
        anchor = anchor.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no existing ancestor for the control state directory",
            )
        })?;
    }
    let relative = run_dir.strip_prefix(anchor).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    let _run_dir = thegn_core::fsperm::ensure_private_dir_beneath(anchor, relative)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        control_socket_candidates_from, prepare_run_dir, publish_candidates_at,
        publish_for_socket_at, publish_socket_candidates_to, run_dir, socket_dir,
    };
    use std::path::{Path, PathBuf};

    #[derive(Debug, Default)]
    struct RecordingSink {
        sockets: Vec<PathBuf>,
        protected: Vec<PathBuf>,
    }

    impl super::PublishSink for RecordingSink {
        fn control_socket_dir(&mut self, dir: PathBuf) {
            if !self.sockets.contains(&dir) {
                self.sockets.push(dir);
            }
        }

        fn protected_state_dir(&mut self, dir: PathBuf) {
            if !self.protected.contains(&dir) {
                self.protected.push(dir);
            }
        }
    }

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

    #[test]
    fn publisher_prepares_the_run_directory_before_publishing() {
        let state = tempfile::tempdir_in("/tmp").unwrap();
        let socket = state.path().join("runtime/daemon.sock");
        let state_dir = state.path().join("state/thegn");
        let mut sink = RecordingSink::default();
        publish_for_socket_at(&socket, &state_dir, &mut sink);
        assert!(run_dir(&state_dir).is_dir());
        assert_eq!(sink.protected, vec![run_dir(&state_dir)]);
        assert_eq!(sink.sockets, vec![state.path().join("runtime")]);
    }

    #[test]
    fn candidates_add_the_predictable_dir_after_a_relocation() {
        let predictable = PathBuf::from("/tmp/thegn-1000-abcd/daemon.sock");
        let relocated =
            PathBuf::from("/tmp/thegn-1000-abcd-0123456789abcdef0123456789abcdef/daemon.sock");
        assert_eq!(
            control_socket_candidates_from(relocated.clone(), predictable.clone()),
            vec![relocated, predictable]
        );
    }

    #[test]
    fn candidates_are_only_the_resolved_socket_without_a_relocation() {
        let same = PathBuf::from("/tmp/thegn-1000-abcd/daemon.sock");
        assert_eq!(
            control_socket_candidates_from(same.clone(), same.clone()),
            vec![same]
        );
        for resolved in [
            "/run/user/1000/thegn-abcd.sock",
            "/home/u/.thegn/daemon.sock",
            "/tmp/thegn-1000-abcdX/daemon.sock",
            "/var/tmp/thegn-1000-abcd-0123456789abcdef0123456789abcdef/daemon.sock",
        ] {
            let resolved = PathBuf::from(resolved);
            assert_eq!(
                control_socket_candidates_from(
                    resolved.clone(),
                    PathBuf::from("/tmp/thegn-1000-abcd/daemon.sock")
                ),
                vec![resolved]
            );
        }
    }

    #[test]
    fn published_set_after_a_relocation_is_complete_in_every_process() {
        let state = tempfile::tempdir_in("/tmp").unwrap();
        let state_dir = state.path().join("state/thegn");
        let predictable = PathBuf::from("/tmp/thegn-1000-abcd/daemon.sock");
        let relocated =
            PathBuf::from("/tmp/thegn-1000-abcd-0123456789abcdef0123456789abcdef/daemon.sock");
        let candidates = control_socket_candidates_from(relocated.clone(), predictable.clone());
        let mut compositor = RecordingSink::default();
        publish_candidates_at(&candidates, &state_dir, &mut compositor);
        publish_for_socket_at(&relocated, &state_dir, &mut compositor);

        let mut cli = RecordingSink::default();
        publish_candidates_at(&candidates, &state_dir, &mut cli);

        let mut daemon = RecordingSink::default();
        publish_socket_candidates_to(&mut daemon, relocated, predictable.clone(), &state_dir);

        let mut expected_sockets = vec![
            PathBuf::from("/tmp/thegn-1000-abcd"),
            PathBuf::from("/tmp/thegn-1000-abcd-0123456789abcdef0123456789abcdef"),
        ];
        let mut expected_protected = vec![run_dir(&state_dir)];
        expected_sockets.sort();
        expected_protected.sort();
        let sink_report = format!("compositor={compositor:?}, CLI={cli:?}, daemon={daemon:?}");
        for (process, sink) in [
            ("compositor", &mut compositor),
            ("CLI", &mut cli),
            ("daemon", &mut daemon),
        ] {
            sink.sockets.sort();
            sink.protected.sort();
            assert_eq!(
                sink.sockets, expected_sockets,
                "{process} sockets: {sink_report}"
            );
            assert_eq!(
                sink.protected, expected_protected,
                "{process} protected paths: {sink_report}"
            );
        }
    }
}
