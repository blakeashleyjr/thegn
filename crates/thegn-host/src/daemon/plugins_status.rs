//! Read the UI-published plugin counters for the control API.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use thegn_core::plugin_status::{MAX_STATUS_BYTES, STATUS_FILE};
use thegn_svc::control::PluginsStatusReply;

/// Read and validate the UI's published snapshot under `state_dir`.
/// Every failure is represented as `published: false`.
pub(crate) fn read_status(state_dir: &Path) -> PluginsStatusReply {
    let path = super::control_publish::run_dir(state_dir).join(STATUS_FILE);
    let result = (|| {
        let file = thegn_core::fsperm::open_regular_file_nofollow(&path)?;
        let mut bytes = Vec::new();
        file.take((MAX_STATUS_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    })();
    let Ok(bytes) = result else {
        return PluginsStatusReply::default();
    };
    let snapshot = match thegn_core::plugin_status::decode(&bytes) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::debug!(target: "thegn::plugin", %error, "invalid published plugin status snapshot");
            return PluginsStatusReply::default();
        }
    };
    PluginsStatusReply {
        published: true,
        live: crate::platform::pid_alive(i64::from(snapshot.pid)),
        pid: Some(snapshot.pid),
        published_at_ms: Some(snapshot.published_at_ms),
        rows: snapshot.rows,
    }
}

pub(crate) fn production_state_dir() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn")
}

#[cfg(test)]
mod tests {
    use super::read_status;
    use std::path::Path;
    use thegn_core::plugin_status::{
        MAX_STATUS_BYTES, PluginStatusRow, PluginStatusSnapshot, STATUS_FILE, STATUS_SCHEMA,
    };

    fn publish(state_dir: &Path, bytes: &[u8]) {
        let run_dir = super::super::control_publish::run_dir(state_dir);
        thegn_core::fsperm::ensure_private_dir(&run_dir).unwrap();
        thegn_core::fsperm::open_private_dir(&run_dir)
            .unwrap()
            .write_owner_only_atomic(STATUS_FILE, bytes)
            .unwrap();
    }

    fn snapshot(pid: u32) -> PluginStatusSnapshot {
        PluginStatusSnapshot {
            schema: STATUS_SCHEMA,
            pid,
            published_at_ms: 1234,
            rows: vec![PluginStatusRow {
                plugin: "example".into(),
                in_flight: 2,
                rejected: 3,
                timed_out: 4,
                lost: 5,
                queue_drops: 6,
                coalesced: 7,
                bridge: Default::default(),
            }],
        }
    }

    #[test]
    fn read_status_returns_unpublished_when_the_file_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(read_status(temp.path()), Default::default());
    }

    #[test]
    fn read_status_decodes_a_published_snapshot_and_flags_a_dead_pid_not_live() {
        let temp = tempfile::tempdir().unwrap();
        let encoded = thegn_core::plugin_status::encode(&snapshot(u32::MAX)).unwrap();
        publish(temp.path(), &encoded);
        let status = read_status(temp.path());
        assert!(status.published);
        assert!(!status.live);
        assert_eq!(status.pid, Some(u32::MAX));
        assert_eq!(status.published_at_ms, Some(1234));
        assert_eq!(status.rows[0].plugin, "example");

        let current = tempfile::tempdir().unwrap();
        let encoded = thegn_core::plugin_status::encode(&snapshot(std::process::id())).unwrap();
        publish(current.path(), &encoded);
        assert!(read_status(current.path()).live);
    }

    #[test]
    fn read_status_refuses_oversized_wrong_schema_and_garbage_files() {
        let oversized = tempfile::tempdir().unwrap();
        publish(oversized.path(), &vec![b'x'; MAX_STATUS_BYTES + 1]);
        let status = read_status(oversized.path());
        assert!(!status.published);
        assert!(status.rows.is_empty());

        let wrong_schema = tempfile::tempdir().unwrap();
        let bytes = serde_json::to_vec(&PluginStatusSnapshot {
            schema: STATUS_SCHEMA + 1,
            ..Default::default()
        })
        .unwrap();
        publish(wrong_schema.path(), &bytes);
        let status = read_status(wrong_schema.path());
        assert!(!status.published);
        assert!(status.rows.is_empty());

        let garbage = tempfile::tempdir().unwrap();
        publish(garbage.path(), b"not json");
        let status = read_status(garbage.path());
        assert!(!status.published);
        assert!(status.rows.is_empty());
    }

    #[test]
    fn read_status_refuses_a_symlinked_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = super::super::control_publish::run_dir(temp.path());
        std::fs::create_dir_all(&run_dir).unwrap();
        let target = temp.path().join("outside.json");
        std::fs::write(
            &target,
            thegn_core::plugin_status::encode(&snapshot(1)).unwrap(),
        )
        .unwrap();
        let link = run_dir.join(STATUS_FILE);
        if thegn_core::fsperm::create_symlink(&target, &link, false).is_err() {
            return;
        }
        let status = read_status(temp.path());
        assert!(!status.published);
        assert!(status.rows.is_empty());
    }
}
