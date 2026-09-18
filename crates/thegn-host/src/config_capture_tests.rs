use super::*;
use crate::state_host_capture::{StateHostCapture, StateHostReadError};
use std::cell::RefCell;

#[cfg(target_os = "linux")]
use rusqlite::Connection;

struct Sources {
    bodies: BTreeMap<PathBuf, Result<Option<Vec<u8>>, ConfigFileReadError>>,
    reads: RefCell<Vec<PathBuf>>,
}

impl Sources {
    fn empty() -> Self {
        Self {
            bodies: BTreeMap::new(),
            reads: RefCell::new(Vec::new()),
        }
    }
}

impl ConfigSourceReader for Sources {
    fn read_bounded(&self, path: &Path, _: usize) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
        self.reads.borrow_mut().push(path.to_owned());
        self.bodies.get(path).cloned().unwrap_or(Ok(None))
    }
}

fn seed(values: &[(&str, &str)], overrides: &[String]) -> ConfigCaptureSeed {
    let cwd = std::env::temp_dir().join("thegn-capture-test-cwd");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let mut vars: BTreeMap<String, OsString> = [
        (keys.home.to_owned(), cwd.join("home").into_os_string()),
        (keys.config.to_owned(), cwd.join("config").into_os_string()),
        (keys.state.to_owned(), cwd.join("state").into_os_string()),
        ("THEGN_DIR".to_owned(), cwd.join("app").into_os_string()),
    ]
    .into_iter()
    .collect();
    vars.extend(
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(*value))),
    );
    ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: None,
            profile_paths: None,
            overrides,
        },
        cwd,
        |key| vars.get(key).cloned(),
        Config::default,
    )
    .unwrap()
}

fn load_empty(
    seed: &ConfigCaptureSeed,
    sources: &Sources,
) -> Result<CapturedConfig, CaptureFailure> {
    seed.load_with(sources, || Ok(StateHostCapture::Absent))
}

#[test]
fn frozen_profile_environment_paths_and_cli_are_not_reread() {
    let cwd = std::env::temp_dir().join("thegn-capture-frozen-cwd");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let values = RefCell::new(BTreeMap::from([
        (keys.home.to_owned(), OsString::from("home")),
        (keys.config.to_owned(), OsString::from("config")),
        (keys.state.to_owned(), OsString::from("state")),
        ("THEGN_DIR".to_owned(), OsString::from("app")),
        ("THEGN_PROFILE".to_owned(), OsString::from("work")),
        ("THEGN_BASE_BRANCH".to_owned(), OsString::from("captured")),
    ]));
    let reads = RefCell::new(BTreeMap::<String, usize>::new());
    let overrides = vec!["base_branch=cli".to_owned()];
    let captured = ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: None,
            profile_paths: None,
            overrides: &overrides,
        },
        cwd,
        |key| {
            *reads.borrow_mut().entry(key.to_owned()).or_default() += 1;
            values.borrow().get(key).cloned()
        },
        Config::default,
    )
    .unwrap();
    values.borrow_mut().clear();

    let mut sources = Sources::empty();
    sources.bodies.insert(
        captured.base.path.clone(),
        Ok(Some(b"branch_prefix = 'captured/'\n".to_vec())),
    );
    sources.bodies.insert(
        captured.profile.as_ref().unwrap().path.clone(),
        Ok(Some(Vec::new())),
    );
    let admitted = load_empty(&captured, &sources).unwrap();
    assert_eq!(admitted.config().base_branch, "cli");
    assert_eq!(admitted.profile_name(), "work");
    assert!(reads.borrow().values().all(|count| *count == 1));
    assert_eq!(sources.reads.borrow().len(), 2);
}

#[test]
fn only_absent_implicit_base_selects_first_run_defaults() {
    let captured = seed(&[], &[]);
    let sources = Sources::empty();
    let admitted = load_empty(&captured, &sources).unwrap();
    assert_eq!(
        admitted.health(),
        thegn_core::config_admission::AdmissionHealth::FirstRunDefault
    );

    let explicit = ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: Some(Path::new("explicit.toml")),
            profile: None,
            profile_paths: None,
            overrides: &[],
        },
        std::env::temp_dir(),
        |_| None,
        Config::default,
    )
    .unwrap();
    assert!(matches!(
        load_empty(&explicit, &Sources::empty()),
        Err(CaptureFailure::Admission(
            ConfigAdmissionError::ExplicitPathMissing
        ))
    ));
}

#[test]
fn selected_profile_missing_is_fatal_and_not_empty_overlay() {
    let captured = seed(&[("THEGN_PROFILE", "work")], &[]);
    let mut sources = Sources::empty();
    sources.bodies.insert(
        captured.base.path.clone(),
        Ok(Some(b"branch_prefix = 'base/'\n".to_vec())),
    );
    assert!(matches!(
        load_empty(&captured, &sources),
        Err(CaptureFailure::Admission(
            ConfigAdmissionError::ProfileInvalid
        ))
    ));
}

#[test]
fn selected_empty_profile_is_distinct_from_missing_and_reaches_admission() {
    let captured = seed(&[("THEGN_PROFILE", "work")], &[]);
    let mut sources = Sources::empty();
    sources.bodies.insert(
        captured.base.path.clone(),
        Ok(Some(b"branch_prefix = 'base/'\n".to_vec())),
    );
    sources.bodies.insert(
        captured.profile.as_ref().unwrap().path.clone(),
        Ok(Some(Vec::new())),
    );
    let state_reads = Cell::new(0);
    let admitted = captured
        .load_with(&sources, || {
            state_reads.set(state_reads.get() + 1);
            Ok(StateHostCapture::Absent)
        })
        .unwrap();
    assert_eq!(admitted.profile_name(), "work");
    assert_eq!(admitted.config().branch_prefix, "base/");
    assert_eq!(state_reads.get(), 1);
}

#[test]
fn selected_profile_read_error_refuses_before_sqlite_capture() {
    let captured = seed(&[("THEGN_PROFILE", "work")], &[]);
    let mut sources = Sources::empty();
    sources.bodies.insert(
        captured.base.path.clone(),
        Ok(Some(b"branch_prefix = 'base/'\n".to_vec())),
    );
    sources.bodies.insert(
        captured.profile.as_ref().unwrap().path.clone(),
        Err(ConfigFileReadError::Unavailable),
    );
    let state_reads = Cell::new(0);
    let result = captured.load_with(&sources, || {
        state_reads.set(state_reads.get() + 1);
        Ok(StateHostCapture::Absent)
    });
    assert!(matches!(
        result,
        Err(CaptureFailure::Source(ConfigFileReadError::Unavailable))
    ));
    assert_eq!(state_reads.get(), 0);
}

#[test]
fn cli_profile_wins_and_pre_resolved_roots_are_not_double_rerooted() {
    let cwd = std::env::temp_dir().join("thegn-capture-profile-contract");
    let base = cwd.join("app");
    let selected_root = base.join("profiles/cli");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let vars = BTreeMap::from([
        (keys.home.to_owned(), OsString::from(cwd.join("home"))),
        (keys.config.to_owned(), OsString::from(cwd.join("config"))),
        (keys.state.to_owned(), OsString::from(cwd.join("state"))),
        (
            "THEGN_DIR".to_owned(),
            selected_root.clone().into_os_string(),
        ),
        ("THEGN_PROFILE".to_owned(), OsString::from("environment")),
    ]);
    let overrides = [];
    let profile_paths = thegn_core::profile::ProfilePaths {
        name: "cli".to_owned(),
        root: selected_root.clone(),
    };
    let captured = ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: Some("cli"),
            profile_paths: Some(&profile_paths),
            overrides: &overrides,
        },
        cwd,
        |key| vars.get(key).cloned(),
        Config::default,
    )
    .unwrap();
    assert_eq!(captured.profile_name, "cli");
    assert_eq!(captured.app_root, selected_root);
    assert_eq!(
        captured.state_db,
        captured.app_root.join("state/thegn/thegn.db")
    );
    assert!(!captured.app_root.ends_with("profiles/cli/profiles/cli"));
}

#[test]
fn selected_profile_path_bound_is_checked_after_join() {
    let cwd = std::env::temp_dir().join("thegn-capture-profile-bounds");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let long_config = cwd.join("config");
    let selected_root = cwd.join("app");
    let vars = BTreeMap::from([
        (keys.home.to_owned(), OsString::from(cwd.join("home"))),
        (keys.config.to_owned(), long_config.clone().into_os_string()),
        (keys.state.to_owned(), OsString::from(cwd.join("state"))),
        (
            "THEGN_DIR".to_owned(),
            selected_root.clone().into_os_string(),
        ),
        ("THEGN_PROFILE".to_owned(), OsString::from("work")),
    ]);
    let long_root = PathBuf::from(format!("/{}", "p".repeat(MAX_PATH_BYTES)));
    let profile_paths = thegn_core::profile::ProfilePaths {
        name: "work".to_owned(),
        root: long_root,
    };
    let result = ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: Some("work"),
            profile_paths: Some(&profile_paths),
            overrides: &[],
        },
        cwd,
        |key| vars.get(key).cloned(),
        Config::default,
    );
    // The complete joined path, rather than only XDG_CONFIG_HOME, is the
    // retained identity bound.  Keep this assertion source-only and exact at
    // the accepted path budget boundary.
    assert!(matches!(result, Err(CaptureInputError::InvalidPath)));
}

#[cfg(target_os = "linux")]
fn native_linux_seed(dir: &tempfile::TempDir) -> ConfigCaptureSeed {
    let keys = crate::platform::config_file_capture::native_path_keys();
    let config = dir.path().join("config");
    let state = dir.path().join("state");
    let home = dir.path().join("home");
    let app = dir.path().join("app");
    std::fs::create_dir_all(config.join("thegn")).unwrap();
    std::fs::create_dir_all(state.join("thegn")).unwrap();
    let vars = BTreeMap::from([
        (keys.home.to_owned(), home.clone().into_os_string()),
        (keys.config.to_owned(), config.clone().into_os_string()),
        (keys.state.to_owned(), state.clone().into_os_string()),
        ("THEGN_DIR".to_owned(), app.clone().into_os_string()),
    ]);
    ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: None,
            profile_paths: None,
            overrides: &[],
        },
        dir.path().to_owned(),
        |key| vars.get(key).cloned(),
        Config::default,
    )
    .unwrap()
}

#[cfg(target_os = "linux")]
fn native_fixture() -> Option<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("thegn-config-integration-")
        .tempdir_in("/dev/shm")
        .unwrap_or_else(|error| panic!("Linux integration fixture setup failed: {error}"));
    let probe = dir.path().join("probe");
    std::fs::write(&probe, b"probe").unwrap();
    match crate::platform::config_file_capture::Reader.read_bounded(&probe, 64) {
        Ok(Some(_)) => Some(dir),
        Err(ConfigFileReadError::Unavailable) => {
            eprintln!(
                "SKIP: Linux config integration cannot exercise the supported /dev/shm tmpfs"
            );
            None
        }
        other => panic!("unexpected native config fixture probe: {other:?}"),
    }
}

#[cfg(target_os = "linux")]
fn create_host_db(path: &Path, wal: bool) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(&format!(
            "PRAGMA user_version={}; CREATE TABLE hosts(host_id TEXT PRIMARY KEY,name TEXT,config_json TEXT);",
            thegn_core::db::SCHEMA_VERSION
        ))
        .unwrap();
    if wal {
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO hosts VALUES ('id','integration-host','{}');",
            )
            .unwrap();
    }
    connection
}

#[cfg(target_os = "linux")]
#[test]
fn load_once_uses_private_config_and_wal_fixtures_and_fails_closed() {
    let Some(dir) = native_fixture() else { return };
    let seed = native_linux_seed(&dir);
    std::fs::write(&seed.base.path, b"branch_prefix = 'captured/'\n").unwrap();
    let writer = create_host_db(&seed.state_db, true);
    let admitted = seed.load_once().unwrap();
    assert_eq!(admitted.config().branch_prefix, "captured/");
    drop(writer);

    // Every failure below travels through the real ConfigCaptureSeed::load_once
    // path. None can be reclassified as an absent/empty host snapshot.
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(seed.state_db.with_extension(format!("db{suffix}")));
    }
    std::fs::write(&seed.state_db, b"not sqlite").unwrap();
    assert!(matches!(
        seed.load_once(),
        Err(CaptureFailure::State(StateHostReadError::Database(_)))
    ));

    let _ = std::fs::remove_file(&seed.state_db);
    let wrong = Connection::open(&seed.state_db).unwrap();
    wrong
        .execute_batch(
            "PRAGMA user_version=67; CREATE TABLE hosts(host_id TEXT PRIMARY KEY,name TEXT,config_json TEXT);",
        )
        .unwrap();
    drop(wrong);
    assert!(matches!(
        seed.load_once(),
        Err(CaptureFailure::State(StateHostReadError::Database(_)))
    ));

    let _ = std::fs::remove_file(&seed.state_db);
    std::fs::write(seed.state_db.with_extension("db-wal"), b"orphan").unwrap();
    assert!(matches!(
        seed.load_once(),
        Err(CaptureFailure::State(StateHostReadError::OrphanedSidecar))
    ));

    let _ = std::fs::remove_file(seed.state_db.with_extension("db-wal"));
    let _ = std::fs::remove_file(&seed.state_db);
    let locked = create_host_db(&seed.state_db, false);
    locked.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(matches!(
        seed.load_once(),
        Err(CaptureFailure::State(StateHostReadError::Database(_)))
    ));
    locked.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn invalid_utf8_oversized_and_source_failures_are_typed() {
    let captured = seed(&[], &[]);
    let mut sources = Sources::empty();
    sources
        .bodies
        .insert(captured.base.path.clone(), Ok(Some(vec![0xff])));
    assert!(matches!(
        load_empty(&captured, &sources),
        Err(CaptureFailure::Admission(ConfigAdmissionError::InvalidUtf8))
    ));
    sources.bodies.insert(
        captured.base.path.clone(),
        Err(ConfigFileReadError::TooLarge),
    );
    assert!(matches!(
        load_empty(&captured, &sources),
        Err(CaptureFailure::Source(ConfigFileReadError::TooLarge))
    ));
}

#[test]
fn host_capture_failure_is_not_replaced_by_empty_hosts() {
    let captured = seed(&[], &[]);
    let sources = Sources::empty();
    let state_reads = Cell::new(0);
    let result = captured.load_with(&sources, || {
        state_reads.set(state_reads.get() + 1);
        Err(StateHostReadError::Unavailable)
    });
    assert!(matches!(
        result,
        Err(CaptureFailure::State(StateHostReadError::Unavailable))
    ));
    assert_eq!(state_reads.get(), 1);
}
