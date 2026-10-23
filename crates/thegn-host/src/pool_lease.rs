//! Process-shared advisory leases for warm-pool reconciliation and provisioning.

use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum LeaseError {
    Busy,
    Io(std::io::Error),
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("pool lease is busy"),
            Self::Io(error) => write!(f, "cannot establish pool lease: {error}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// Holds an advisory file lock until dropped. Pool lock files are never removed:
/// unlinking one could let another process lock a different inode at the same path.
pub(crate) struct Lease {
    _file: File,
}

fn lock_dir() -> PathBuf {
    #[cfg(test)]
    {
        thread_local! {
            static DIR: tempfile::TempDir = tempfile::tempdir().expect("test pool lock dir");
        }
        DIR.with(|dir| dir.path().join("locks").join("pool"))
    }
    #[cfg(not(test))]
    {
        thegn_core::util::xdg_state_home()
            .join("thegn")
            .join("locks")
            .join("pool")
    }
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    crate::platform::restrict_dir_owner_only(dir);
    Ok(())
}

fn lock_path_in(dir: &Path, key: &str) -> PathBuf {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    dir.join(hex)
}

pub(crate) fn pool_lease(repo: &str, env: &str) -> Result<Lease, LeaseError> {
    pool_lease_in(&lock_dir(), repo, env)
}

fn pool_lease_in(dir: &Path, repo: &str, env: &str) -> Result<Lease, LeaseError> {
    lease_in(dir, &format!("pool\0{repo}\0{env}"))
}

pub(crate) fn spare_lease(name: &str) -> Result<Lease, LeaseError> {
    spare_lease_in(&lock_dir(), name)
}

fn spare_lease_in(dir: &Path, name: &str) -> Result<Lease, LeaseError> {
    lease_in(dir, &format!("spare\0{name}"))
}

fn lease_in(dir: &Path, key: &str) -> Result<Lease, LeaseError> {
    ensure_dir(dir).map_err(LeaseError::Io)?;
    let file = crate::platform::open_lock_file(&lock_path_in(dir, key)).map_err(LeaseError::Io)?;
    match file.try_lock() {
        Ok(()) => Ok(Lease { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(LeaseError::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(LeaseError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::time::{Duration, Instant};

    struct FixtureChild {
        id: String,
        child: Child,
        stdin: Option<ChildStdin>,
    }

    impl Drop for FixtureChild {
        #[expect(
            clippy::disallowed_methods,
            reason = "the fixture child is reaped during guard cleanup"
        )]
        fn drop(&mut self) {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, child_id = %self.id, "could not check pool lease fixture before cleanup")
                }
            }
            if let Err(error) = self.child.kill() {
                tracing::debug!(%error, child_id = %self.id, "could not kill pool lease fixture during cleanup");
            }
            if let Err(error) = self.child.wait() {
                tracing::debug!(%error, child_id = %self.id, "could not reap pool lease fixture during cleanup");
            }
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture child is owned by a reaping guard"
    )]
    fn spawn_lease_fixture(
        lock_dir: &Path,
        report_dir: &Path,
        kind: &str,
        key: &str,
        id: &str,
    ) -> FixtureChild {
        let mut child = Command::new(std::env::current_exe().expect("test executable path"))
            .args([
                "--exact",
                "pool_lease::tests::lease_holder_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("THEGN_POOL_LEASE_FIXTURE_DIR", lock_dir)
            .env("THEGN_POOL_LEASE_FIXTURE_REPORT", report_dir)
            .env("THEGN_POOL_LEASE_FIXTURE_KIND", kind)
            .env("THEGN_POOL_LEASE_FIXTURE_KEY", key)
            .env("THEGN_POOL_LEASE_FIXTURE_ID", id)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pool lease fixture");
        FixtureChild {
            id: id.to_owned(),
            stdin: child.stdin.take(),
            child,
        }
    }

    fn wait_for_report(report_dir: &Path, child: &FixtureChild) -> String {
        let report = report_dir.join(&child.id);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if report.is_file() {
                return std::fs::read_to_string(report).expect("read pool lease fixture report");
            }
            if Instant::now() >= deadline {
                let files = std::fs::read_dir(report_dir)
                    .expect("read pool lease reports")
                    .map(|entry| entry.expect("read pool lease report entry").file_name())
                    .collect::<Vec<_>>();
                panic!(
                    "timed out waiting for pool lease fixture {}: reports={files:?}",
                    child.id
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "fixture children are reaped on every path"
    )]
    fn reap_fixture(child: &mut FixtureChild, kill: bool) {
        child.stdin.take();
        if kill {
            child.child.kill().expect("kill pool lease fixture");
        }
        child.child.wait().expect("reap pool lease fixture");
    }

    #[test]
    fn lease_holder_fixture() {
        let Ok(lock_dir) = std::env::var("THEGN_POOL_LEASE_FIXTURE_DIR") else {
            return;
        };
        let report_dir =
            std::env::var("THEGN_POOL_LEASE_FIXTURE_REPORT").expect("fixture report dir");
        let kind = std::env::var("THEGN_POOL_LEASE_FIXTURE_KIND").expect("fixture kind");
        let key = std::env::var("THEGN_POOL_LEASE_FIXTURE_KEY").expect("fixture key");
        let id = std::env::var("THEGN_POOL_LEASE_FIXTURE_ID").expect("fixture id");
        let (outcome, held_lease) = match kind.as_str() {
            "pool" => match pool_lease_in(Path::new(&lock_dir), &key, "env") {
                Ok(lease) => ("held".to_owned(), Some(lease)),
                Err(LeaseError::Busy) => ("busy".to_owned(), None),
                Err(LeaseError::Io(error)) => (format!("io:{error}"), None),
            },
            "spare" => match spare_lease_in(Path::new(&lock_dir), &key) {
                Ok(lease) => ("held".to_owned(), Some(lease)),
                Err(LeaseError::Busy) => ("busy".to_owned(), None),
                Err(LeaseError::Io(error)) => (format!("io:{error}"), None),
            },
            other => panic!("unknown fixture lease kind: {other}"),
        };
        let report_dir = PathBuf::from(report_dir);
        let temporary = report_dir.join(format!("{id}.tmp"));
        let report = report_dir.join(id);
        std::fs::write(&temporary, outcome.as_bytes()).expect("write fixture report");
        std::fs::rename(temporary, report).expect("publish fixture report");
        if held_lease.is_some() {
            let stdin = std::io::stdin();
            let mut input = Vec::new();
            stdin
                .lock()
                .read_to_end(&mut input)
                .expect("wait for fixture stdin EOF");
        }
        drop(held_lease);
    }

    // The child owns the OS file lock independently of this test process. Its report is
    // published only after acquisition, and stdin EOF is the clean-release signal.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "every fixture child is reaped on every path"
    )]
    fn pool_lease_held_by_another_process_is_busy_until_it_dies() {
        let root = tempfile::tempdir().unwrap();
        let lock_dir = root.path().join("locks");
        let report_dir = root.path().join("reports");
        std::fs::create_dir(&lock_dir).unwrap();
        std::fs::create_dir(&report_dir).unwrap();
        let mut child = spawn_lease_fixture(&lock_dir, &report_dir, "spare", "s1", "owner");
        assert_eq!(wait_for_report(&report_dir, &child), "held");
        assert!(matches!(
            spare_lease_in(&lock_dir, "s1"),
            Err(LeaseError::Busy)
        ));
        reap_fixture(&mut child, true);
        assert!(spare_lease_in(&lock_dir, "s1").is_ok());
    }

    // A live fixture keeps its lock while blocked on stdin; closing stdin lets normal
    // process teardown drop the file lock before the parent retries it.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "every fixture child is reaped on every path"
    )]
    fn pool_lease_clean_exit_releases_the_lease() {
        let root = tempfile::tempdir().unwrap();
        let lock_dir = root.path().join("locks");
        let report_dir = root.path().join("reports");
        std::fs::create_dir(&lock_dir).unwrap();
        std::fs::create_dir(&report_dir).unwrap();
        let mut child = spawn_lease_fixture(&lock_dir, &report_dir, "spare", "s1", "owner");
        assert_eq!(wait_for_report(&report_dir, &child), "held");
        assert!(matches!(
            spare_lease_in(&lock_dir, "s1"),
            Err(LeaseError::Busy)
        ));
        reap_fixture(&mut child, false);
        assert!(spare_lease_in(&lock_dir, "s1").is_ok());
    }

    // Four independent processes start against one shared pool key. File-lock acquisition
    // is exclusive, reports are unordered, and only the holder remains blocked on stdin.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "every fixture child is reaped on every path"
    )]
    fn pool_lease_racing_processes_have_exactly_one_holder() {
        let root = tempfile::tempdir().unwrap();
        let lock_dir = root.path().join("locks");
        let report_dir = root.path().join("reports");
        std::fs::create_dir(&lock_dir).unwrap();
        std::fs::create_dir(&report_dir).unwrap();
        let mut children = (0..4)
            .map(|index| {
                spawn_lease_fixture(
                    &lock_dir,
                    &report_dir,
                    "pool",
                    "repo",
                    &format!("racer-{index}"),
                )
            })
            .collect::<Vec<_>>();
        let reports = children
            .iter()
            .map(|child| wait_for_report(&report_dir, child))
            .collect::<Vec<_>>();
        assert_eq!(
            reports.iter().filter(|report| *report == "held").count(),
            1,
            "reports={reports:?}"
        );
        assert_eq!(
            reports.iter().filter(|report| *report == "busy").count(),
            3,
            "reports={reports:?}"
        );
        assert!(matches!(
            pool_lease_in(&lock_dir, "repo", "env"),
            Err(LeaseError::Busy)
        ));
        for child in &mut children {
            reap_fixture(child, false);
        }
        assert!(pool_lease_in(&lock_dir, "repo", "env").is_ok());
    }

    // Lease keys include the resource kind and name; a different spare and a pool
    // namespace must remain independently acquirable while the child holds spare "a".
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "every fixture child is reaped on every path"
    )]
    fn pool_lease_other_process_key_does_not_contend() {
        let root = tempfile::tempdir().unwrap();
        let lock_dir = root.path().join("locks");
        let report_dir = root.path().join("reports");
        std::fs::create_dir(&lock_dir).unwrap();
        std::fs::create_dir(&report_dir).unwrap();
        let mut child = spawn_lease_fixture(&lock_dir, &report_dir, "spare", "a", "owner");
        assert_eq!(wait_for_report(&report_dir, &child), "held");
        assert!(spare_lease_in(&lock_dir, "b").is_ok());
        assert!(pool_lease_in(&lock_dir, "a", "env").is_ok());
        assert!(matches!(
            spare_lease_in(&lock_dir, "a"),
            Err(LeaseError::Busy)
        ));
        reap_fixture(&mut child, false);
    }

    #[test]
    fn pool_lease_second_holder_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let first = pool_lease_in(dir.path(), "r", "e").unwrap();
        assert!(matches!(
            pool_lease_in(dir.path(), "r", "e"),
            Err(LeaseError::Busy)
        ));
        drop(first);
        assert!(pool_lease_in(dir.path(), "r", "e").is_ok());
    }

    #[test]
    fn pool_lease_keys_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let _pool = pool_lease_in(dir.path(), "r", "e").unwrap();
        assert!(pool_lease_in(dir.path(), "r", "other").is_ok());
        assert!(pool_lease_in(dir.path(), "other", "e").is_ok());
        assert!(spare_lease_in(dir.path(), "r").is_ok());
    }

    #[test]
    fn pool_lease_lock_dir_error_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"not a directory").unwrap();
        assert!(matches!(
            pool_lease_in(&path, "r", "e"),
            Err(LeaseError::Io(_))
        ));
    }
}
