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
            ConfigAdmissionError::ExplicitPathMissing,
            _
        ))
    ));
}

/// A named profile needs no overlay file: nothing creates one, and every
/// child of a named-profile session inherits THEGN_PROFILE. Absent is an
/// empty layer; the selection still changes the admitted revision.
#[test]
fn selected_profile_without_an_overlay_file_is_an_empty_layer() {
    let captured = seed(&[("THEGN_PROFILE", "work")], &[]);
    let mut sources = Sources::empty();
    sources.bodies.insert(
        captured.base.path.clone(),
        Ok(Some(b"branch_prefix = 'base/'\n".to_vec())),
    );
    let admitted = load_empty(&captured, &sources).expect("absent overlay admits");
    assert_eq!(admitted.profile_name(), "work");
    assert_eq!(admitted.config().branch_prefix, "base/");

    let default = seed(&[], &[]);
    let mut default_sources = Sources::empty();
    default_sources.bodies.insert(
        default.base.path.clone(),
        Ok(Some(b"branch_prefix = 'base/'\n".to_vec())),
    );
    let default_admitted = load_empty(&default, &default_sources).unwrap();
    assert_ne!(
        admitted.into_admitted().revision(),
        default_admitted.into_admitted().revision(),
        "the profile selection is part of the revision"
    );
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
fn invalid_process_profile_slug_is_refused_instead_of_selecting_default() {
    let cwd = std::env::temp_dir().join("thegn-capture-invalid-profile");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let vars = BTreeMap::from([
        (keys.home.to_owned(), cwd.join("home").into_os_string()),
        (keys.config.to_owned(), cwd.join("config").into_os_string()),
        (keys.state.to_owned(), cwd.join("state").into_os_string()),
        ("THEGN_DIR".to_owned(), cwd.join("app").into_os_string()),
    ]);
    let result = ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: Some("!!!"),
            profile_paths: None,
            overrides: &[],
        },
        cwd.clone(),
        |key| vars.get(key).cloned(),
        Config::default,
    );
    assert!(matches!(
        result,
        Err(CaptureInputError::ProfileSelector(
            thegn_core::profile::ProfileSelectorError::EmptyNormalized
        ))
    ));
    assert!(
        !cwd.join("app").exists() && !cwd.join("config").exists() && !cwd.join("state").exists(),
        "selector refusal must not create or reroot storage roots"
    );
}

#[test]
fn invalid_environment_process_profile_is_typed_and_redacted() {
    let cwd = std::env::temp_dir().join("thegn-capture-invalid-env-profile");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let vars = BTreeMap::from([
        (keys.home.to_owned(), cwd.join("home").into_os_string()),
        (keys.config.to_owned(), cwd.join("config").into_os_string()),
        (keys.state.to_owned(), cwd.join("state").into_os_string()),
        ("THEGN_DIR".to_owned(), cwd.join("app").into_os_string()),
        ("THEGN_PROFILE".to_owned(), OsString::from("!!!")),
    ]);
    let result = ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: None,
            profile_paths: None,
            overrides: &[],
        },
        cwd.clone(),
        |key| vars.get(key).cloned(),
        Config::default,
    );
    let error = match result {
        Ok(_) => panic!("invalid environment selector unexpectedly admitted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        CaptureInputError::ProfileSelector(
            thegn_core::profile::ProfileSelectorError::EmptyNormalized
        )
    ));
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
fn pre_resolved_profile_mismatch_is_refused_without_using_the_frozen_root() {
    let cwd = std::env::temp_dir().join("thegn-capture-profile-mismatch");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let vars = BTreeMap::from([
        (keys.home.to_owned(), OsString::from(cwd.join("home"))),
        (keys.config.to_owned(), OsString::from(cwd.join("config"))),
        (keys.state.to_owned(), OsString::from(cwd.join("state"))),
        ("THEGN_DIR".to_owned(), OsString::from(cwd.join("app"))),
    ]);
    let frozen = thegn_core::profile::ProfilePaths {
        name: "work".to_owned(),
        root: cwd.join("app/profiles/work"),
    };
    let result = ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: Some("other"),
            profile_paths: Some(&frozen),
            overrides: &[],
        },
        cwd,
        |key| vars.get(key).cloned(),
        Config::default,
    );
    assert!(matches!(
        result,
        Err(CaptureInputError::ProfileBindingMismatch)
    ));
}

#[test]
fn keybind_profile_override_does_not_change_captured_storage_profile() {
    let overrides = vec!["profile=vim".to_owned()];
    let captured = seed(&[("THEGN_PROFILE", "work")], &overrides);
    let mut sources = Sources::empty();
    sources.bodies.insert(
        captured.base.path.clone(),
        Ok(Some(b"branch_prefix = 'base/'\n".to_vec())),
    );
    sources.bodies.insert(
        captured.profile.as_ref().unwrap().path.clone(),
        Ok(Some(Vec::new())),
    );

    let admitted = load_empty(&captured, &sources).unwrap();
    assert_eq!(admitted.profile_name(), "work");
    assert_eq!(admitted.config().profile, "vim");
    assert!(captured.app_root.ends_with("profiles/work"));
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
    let long_root = PathBuf::from(format!("/{}", "p".repeat(MAX_PATH_BYTES - 5)));
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
fn native_fixture() -> tempfile::TempDir {
    // The production DB adapter checks every ancestor from /. A shared /tmp
    // or /dev/shm fixture must be refused, even when its leaf is private.
    // Keep this real integration fixture below the user's private home. An
    // unsupported home filesystem is a visible test failure, not fake coverage.
    let home = std::env::var_os("HOME").expect("Linux integration fixture needs HOME");
    tempfile::Builder::new()
        .prefix(".thegn-config-integration-")
        .tempdir_in(home)
        .expect("create private configuration integration fixture")
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
    let dir = native_fixture();
    let seed = native_linux_seed(&dir);
    std::fs::write(&seed.base.path, b"branch_prefix = 'captured/'\n").unwrap();
    let writer = create_host_db(&seed.state_db, true);
    let admitted = seed.load_once().unwrap();
    assert_eq!(admitted.config().branch_prefix, "captured/");
    assert!(admitted.config().host.contains_key("integration-host"));
    drop(writer);

    // Every failure below travels through the real ConfigCaptureSeed::load_once
    // path. None can be reclassified as an absent/empty host snapshot.
    for suffix in ["-wal", "-shm", "-journal"] {
        std::fs::remove_file(seed.state_db.with_extension(format!("db{suffix}")))
            .unwrap_or_default(); // best-effort: fixture cleanup; never hides an expected refusal
    }
    std::fs::write(&seed.state_db, b"not sqlite").unwrap();
    assert!(matches!(
        seed.load_once(),
        Err(CaptureFailure::State(StateHostReadError::Database(_)))
    ));

    std::fs::remove_file(&seed.state_db).unwrap_or_default(); // best-effort: fixture cleanup; refusal is asserted below
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

    std::fs::remove_file(&seed.state_db).unwrap_or_default(); // best-effort: fixture cleanup; refusal is asserted below
    std::fs::write(seed.state_db.with_extension("db-wal"), b"orphan").unwrap();
    assert!(matches!(
        seed.load_once(),
        Err(CaptureFailure::State(StateHostReadError::OrphanedSidecar))
    ));

    std::fs::remove_file(seed.state_db.with_extension("db-wal")).unwrap_or_default(); // best-effort: fixture cleanup; orphan refusal is already asserted
    std::fs::remove_file(&seed.state_db).unwrap_or_default(); // best-effort: fixture cleanup; busy refusal is asserted below
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
        Err(CaptureFailure::Admission(
            ConfigAdmissionError::InvalidUtf8,
            _
        ))
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
fn final_link_refusal_is_actionable_but_does_not_disclose_source_contents() {
    let error = CaptureFailure::Source(ConfigFileReadError::FinalLinkUnsupported);
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("ordinary private file"));
    assert!(!diagnostic.contains("private config contents"));
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

/// `THEGN_AUTO_REMOVE_WORKTREE=` (an empty export) was ignored by the legacy
/// ProcessEnv; admission must not turn it into a refusal, and an empty
/// `THEGN_BRANCH_PREFIX=` must not override the file with "".
#[test]
fn empty_and_whitespace_environment_values_are_unset() {
    let captured = seed(
        &[
            ("THEGN_AUTO_REMOVE_WORKTREE", ""),
            ("THEGN_SANDBOX_ENABLED", "  "),
            ("THEGN_BRANCH_PREFIX", ""),
        ],
        &[],
    );
    let mut sources = Sources::empty();
    sources.bodies.insert(
        captured.base.path.clone(),
        Ok(Some(b"branch_prefix = 'file/'\n".to_vec())),
    );
    let admitted = load_empty(&captured, &sources).expect("empty exports are unset");
    assert_eq!(admitted.config().branch_prefix, "file/");
}
