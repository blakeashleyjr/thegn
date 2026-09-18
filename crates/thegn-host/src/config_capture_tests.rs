use super::*;
use crate::state_host_capture::{StateHostCapture, StateHostReadError};
use std::cell::RefCell;

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
