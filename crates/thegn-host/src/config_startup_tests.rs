use super::*;
use crate::config_capture::{CapturedConfig, ConfigFileReadError, ConfigSourceReader};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::config_admission::ConfigAdmissionError;
use thegn_core::config_admission_store::{StaleConfigReason, StoreHealth};

/// In-memory sources keyed by path; the body can be swapped between reloads.
struct Sources {
    bodies: RefCell<BTreeMap<PathBuf, Vec<u8>>>,
}

impl ConfigSourceReader for Sources {
    fn read_bounded(&self, path: &Path, _: usize) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
        Ok(self.bodies.borrow().get(path).cloned())
    }
}

fn seed() -> ConfigCaptureSeed {
    let cwd = std::env::temp_dir().join("thegn-startup-test-cwd");
    let keys = crate::platform::config_file_capture::native_path_keys();
    let vars: BTreeMap<String, OsString> = [
        (keys.home.to_owned(), cwd.join("home").into_os_string()),
        (keys.config.to_owned(), cwd.join("config").into_os_string()),
        (keys.state.to_owned(), cwd.join("state").into_os_string()),
        ("THEGN_DIR".to_owned(), cwd.join("app").into_os_string()),
    ]
    .into_iter()
    .collect();
    ConfigCaptureSeed::capture_with(
        CapturedCliInputs {
            config: None,
            profile: None,
            profile_paths: None,
            overrides: &[],
        },
        cwd,
        |key| vars.get(key).cloned(),
        Config::default,
    )
    .unwrap()
}

fn base_path() -> PathBuf {
    std::env::temp_dir()
        .join("thegn-startup-test-cwd")
        .join("config")
        .join("thegn/config.toml")
}

fn admit_from<'a>(
    sources: &'a Sources,
    host_reads: &'a Cell<u32>,
) -> impl Fn(
    &ConfigCaptureSeed,
    thegn_core::db::MigrationActor,
) -> Result<CapturedConfig, CaptureFailure>
+ 'a {
    move |seed, _| {
        seed.admit_staged(sources, |_| {
            host_reads.set(host_reads.get() + 1);
            Ok(HostDefinitionsSnapshot::empty(
                thegn_core::db::SCHEMA_VERSION,
            ))
        })
    }
}

fn sources(body: &str) -> Sources {
    Sources {
        bodies: RefCell::new([(base_path(), body.as_bytes().to_vec())].into()),
    }
}

#[test]
fn startup_publishes_generation_one_from_one_admission() {
    let files = sources("branch_prefix = \"one/\"\n");
    let host_reads = Cell::new(0);
    let (process, published) = ProcessAdmission::admit_initial(
        seed(),
        thegn_core::db::MigrationActor::Controller,
        &admit_from(&files, &host_reads),
    )
    .unwrap();
    assert_eq!(published.revision().generation, 1);
    assert_eq!(published.config().branch_prefix, "one/");
    assert_eq!(host_reads.get(), 1, "hosts are captured exactly once");
    assert!(process.store().authorize(&published.revision()).is_ok());
}

#[test]
fn invalid_startup_config_never_reaches_the_host_store_or_publishes() {
    for body in [
        "branch_prefix = \n",                            // parse error
        "no_such_key = 1\n",                             // unknown key (schema)
        "[ui]\nsidebar_workspace_sort = \"sideways\"\n", // bad enum
    ] {
        let files = sources(body);
        let host_reads = Cell::new(0);
        let result = ProcessAdmission::admit_initial(
            seed(),
            thegn_core::db::MigrationActor::Controller,
            &admit_from(&files, &host_reads),
        );
        assert!(
            matches!(result, Err(CaptureFailure::Admission(_))),
            "{body:?} must be refused"
        );
        assert_eq!(
            host_reads.get(),
            0,
            "{body:?}: the state store must not be opened for an invalid trusted layer"
        );
    }
}

#[test]
fn absent_implicit_config_is_the_only_first_run_default() {
    let files = Sources {
        bodies: RefCell::new(BTreeMap::new()),
    };
    let host_reads = Cell::new(0);
    let (_, published) = ProcessAdmission::admit_initial(
        seed(),
        thegn_core::db::MigrationActor::Controller,
        &admit_from(&files, &host_reads),
    )
    .unwrap();
    assert_eq!(
        published.health(),
        thegn_core::config_admission::AdmissionHealth::FirstRunDefault
    );
}

#[test]
fn failed_reload_keeps_last_good_display_only_and_coalesces() {
    let files = sources("branch_prefix = \"one/\"\n");
    let host_reads = Cell::new(0);
    let admit = admit_from(&files, &host_reads);
    let (process, first) =
        ProcessAdmission::admit_initial(seed(), thegn_core::db::MigrationActor::Controller, &admit)
            .unwrap();

    // A truncated save (the partial write a non-atomic editor leaves).
    files
        .bodies
        .borrow_mut()
        .insert(base_path(), b"branch_prefix = \"tw".to_vec());
    assert!(matches!(
        process.reload_with(&admit),
        ReloadOutcome::Failed(CaptureFailure::Admission(
            ConfigAdmissionError::ParseInvalid
        ))
    ));
    assert!(matches!(
        process.reload_with(&admit),
        ReloadOutcome::FailedCoalesced
    ));
    assert_eq!(
        process.store().health(),
        StoreHealth::Degraded {
            generation: 1,
            error: ConfigAdmissionError::ParseInvalid,
            failures: 2,
        }
    );
    // Display keeps generation 1 — never a default and never a partial mix.
    let shown = process.store().display().unwrap();
    assert_eq!(shown.revision(), first.revision());
    assert_eq!(shown.config().branch_prefix, "one/");
    // …but it no longer authorizes anything.
    assert_eq!(
        process
            .store()
            .authorize(&first.revision())
            .unwrap_err()
            .reason,
        StaleConfigReason::Degraded
    );

    // The fixed file publishes generation 2; generation 1 is now stale.
    files
        .bodies
        .borrow_mut()
        .insert(base_path(), b"branch_prefix = \"two/\"\n".to_vec());
    let ReloadOutcome::Published(second) = process.reload_with(&admit) else {
        panic!("a valid reload publishes");
    };
    assert_eq!(second.revision().generation, 2);
    assert_eq!(second.config().branch_prefix, "two/");
    assert_eq!(
        process
            .store()
            .authorize(&first.revision())
            .unwrap_err()
            .reason,
        StaleConfigReason::Superseded
    );
    assert!(process.store().authorize(&second.revision()).is_ok());
}

#[test]
fn host_capture_failure_on_reload_is_degraded_not_empty_hosts() {
    let files = sources("branch_prefix = \"one/\"\n");
    let host_reads = Cell::new(0);
    let (process, first) = ProcessAdmission::admit_initial(
        seed(),
        thegn_core::db::MigrationActor::Controller,
        &admit_from(&files, &host_reads),
    )
    .unwrap();
    let failing = |seed: &ConfigCaptureSeed, _: thegn_core::db::MigrationActor| {
        seed.admit_staged(&files, |_| {
            Err(CaptureFailure::Hosts(HostStoreFailure::NewerSchema {
                observed: 99,
                supported: 68,
            }))
        })
    };
    let ReloadOutcome::Failed(error) = process.reload_with(&failing) else {
        panic!("host failure must be reported");
    };
    assert!(error.to_string().contains("newer than this build"));
    assert!(matches!(
        process.store().health(),
        StoreHealth::Degraded {
            error: ConfigAdmissionError::HostInvalid,
            ..
        }
    ));
    assert!(process.store().authorize(&first.revision()).is_err());
}

#[test]
fn host_store_failure_detail_is_bounded() {
    let detail = bounded_detail(&"x".repeat(10_000));
    assert!(detail.len() <= 240 + '…'.len_utf8());
}

/// A reader that retargets the config symlink while "reading" the target.
#[cfg(unix)]
struct Retarget<'a> {
    link: &'a Path,
    other: &'a Path,
}

#[cfg(unix)]
impl ConfigSourceReader for Retarget<'_> {
    fn read_bounded(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
        let bytes = crate::platform::config_file_capture::Reader.read_bounded(path, limit);
        std::fs::remove_file(self.link).unwrap();
        std::os::unix::fs::symlink(self.other, self.link).unwrap();
        bytes
    }
}

#[cfg(unix)]
#[test]
fn final_link_resolution_reads_the_target_and_refuses_retargeting() {
    use crate::config_capture::FinalLinkResolvingReader;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
    // A private directory: the Linux reader refuses world-writable ancestors
    // such as /tmp, so the fixture lives under the user's home.
    let dir = tempfile::tempdir_in(&home).unwrap();
    let target = dir.path().join("managed.toml");
    let other = dir.path().join("other.toml");
    std::fs::write(&target, b"branch_prefix = \"managed/\"\n").unwrap();
    std::fs::write(&other, b"branch_prefix = \"other/\"\n").unwrap();
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let reader = FinalLinkResolvingReader(crate::platform::config_file_capture::Reader);
    assert_eq!(
        reader.read_bounded(&link, 1024).unwrap().as_deref(),
        Some(&b"branch_prefix = \"managed/\"\n"[..])
    );
    // Absent path: absent source (first-run eligible only when implicit).
    assert_eq!(
        reader.read_bounded(&dir.path().join("missing.toml"), 1024),
        Ok(None)
    );
    // A dangling link exists: it is unreadable, never "absent".
    let dangling = dir.path().join("dangling.toml");
    std::os::unix::fs::symlink(dir.path().join("gone.toml"), &dangling).unwrap();
    assert_eq!(
        reader.read_bounded(&dangling, 1024),
        Err(ConfigFileReadError::Unavailable)
    );
    // Retargeted mid-read: refused as a changed source.
    let retarget = FinalLinkResolvingReader(Retarget {
        link: &link,
        other: &other,
    });
    assert_eq!(
        retarget.read_bounded(&link, 1024),
        Err(ConfigFileReadError::Changed)
    );
}
