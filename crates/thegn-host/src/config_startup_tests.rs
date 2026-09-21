use super::*;
use crate::config_capture::{CapturedConfig, ConfigFileReadError, ConfigSourceReader};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::config_admission_store::{StaleConfigReason, StoreHealth};
use thegn_core::host_definition_snapshot::HostDefinitionsSnapshot;

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
    bool,
) -> Result<CapturedConfig, CaptureFailure>
+ 'a {
    move |seed, _, _| {
        seed.admit_staged(sources, |_| {
            host_reads.set(host_reads.get() + 1);
            Ok(HostLayer::Admitted(HostDefinitionsSnapshot::empty(
                thegn_core::db::SCHEMA_VERSION,
            )))
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
        "branch_prefix = \n", // parse error
        // An unknown key in a security-relevant table still refuses; the
        // relaxed case (unknown key elsewhere) has its own core test.
        "[sandbox]\nno_such_key = 1\n",
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
            matches!(result, Err(CaptureFailure::Admission(..))),
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
    let ReloadOutcome::Failed(CaptureFailure::Admission(error, _)) = process.reload_with(&admit)
    else {
        panic!("a truncated save must be refused and reported once");
    };
    assert!(matches!(
        process.reload_with(&admit),
        ReloadOutcome::FailedCoalesced
    ));
    assert_eq!(
        process.store().health(),
        StoreHealth::Degraded {
            generation: 1,
            error,
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

/// A host layer that cannot be captured (here: a newer schema) must not
/// refuse the process — doctor, logs, notify and the stdio bridges have to
/// keep working — but the host-less generation is display-only: it is never
/// authorized for a launch, and the reason is on the persistent banner.
#[test]
fn unavailable_host_layer_publishes_a_host_less_non_authoritative_generation() {
    let files = sources("branch_prefix = \"one/\"\n");
    let installs = Cell::new(0);
    let unavailable =
        |seed: &ConfigCaptureSeed, _: thegn_core::db::MigrationActor, install: bool| {
            if install {
                installs.set(installs.get() + 1);
            }
            seed.admit_staged(&files, |_| {
                Ok(HostLayer::Unavailable(HostStoreFailure::NewerSchema {
                    observed: 99,
                    supported: 68,
                }))
            })
        };
    let (process, first) = ProcessAdmission::admit_initial(
        seed(),
        thegn_core::db::MigrationActor::Client,
        &unavailable,
    )
    .expect("a newer schema must not refuse startup");
    assert_eq!(first.config().branch_prefix, "one/");
    assert_eq!(
        first.health(),
        thegn_core::config_admission::AdmissionHealth::HostsUnavailable
    );
    assert_eq!(
        process
            .store()
            .authorize(&first.revision())
            .unwrap_err()
            .reason,
        StaleConfigReason::HostsUnavailable
    );
    let banner = process.banner().expect("persistent banner");
    assert!(banner.contains("newer than this build"), "{banner}");
    // The banner must claim only what is actually refused: `require_launchable`
    // covers the daemon's per-launch refresh, not every launch route.
    assert!(banner.contains("agent and tool launches"), "{banner}");
    assert!(!banner.contains("launches are refused: "), "{banner}");

    // Reload never re-installs the migration policy.
    let ReloadOutcome::Published(_) = process.reload_with(&unavailable) else {
        panic!("reload publishes");
    };
    assert_eq!(
        installs.get(),
        1,
        "policy installed exactly once, at startup"
    );

    // Recovery: once the host layer is readable, authority returns.
    let host_reads = Cell::new(0);
    let ReloadOutcome::Published(healthy) = process.reload_with(&admit_from(&files, &host_reads))
    else {
        panic!("recovered reload publishes");
    };
    assert!(process.store().authorize(&healthy.revision()).is_ok());
    assert!(process.banner().is_none());
}

/// Two different failures of the same category are both reported, and the
/// banner persists (and names the latest) until a reload succeeds.
#[test]
fn distinct_reload_failures_are_reported_and_the_banner_persists() {
    let files = sources("branch_prefix = \"one/\"\n");
    let host_reads = Cell::new(0);
    let admit = admit_from(&files, &host_reads);
    let (process, _) =
        ProcessAdmission::admit_initial(seed(), thegn_core::db::MigrationActor::Controller, &admit)
            .unwrap();
    files
        .bodies
        .borrow_mut()
        .insert(base_path(), b"[sandbox]\nno_such_key_one = 1\n".to_vec());
    assert!(matches!(
        process.reload_with(&admit),
        ReloadOutcome::Failed(_)
    ));
    assert!(matches!(
        process.reload_with(&admit),
        ReloadOutcome::FailedCoalesced
    ));
    files
        .bodies
        .borrow_mut()
        .insert(base_path(), b"[sandbox]\nno_such_key_two = 1\n".to_vec());
    assert!(
        matches!(process.reload_with(&admit), ReloadOutcome::Failed(_)),
        "a different problem of the same category is new information"
    );
    let banner = process.banner().expect("banner persists while degraded");
    assert!(banner.contains("no_such_key_two"), "{banner}");
    files
        .bodies
        .borrow_mut()
        .insert(base_path(), b"branch_prefix = \"two/\"\n".to_vec());
    assert!(matches!(
        process.reload_with(&admit),
        ReloadOutcome::Published(_)
    ));
    assert!(process.banner().is_none());
}

/// A reload that changes `[database]` still publishes; the change is a
/// restart-required diagnostic, never a refusal of later launches.
#[test]
fn database_change_on_reload_is_not_a_refusal() {
    let files = sources("branch_prefix = \"one/\"\n");
    let host_reads = Cell::new(0);
    let admit = admit_from(&files, &host_reads);
    let (process, _) =
        ProcessAdmission::admit_initial(seed(), thegn_core::db::MigrationActor::Controller, &admit)
            .unwrap();
    files.bodies.borrow_mut().insert(
        base_path(),
        b"[database]\nmigration_authority = \"any\"\n".to_vec(),
    );
    let ReloadOutcome::Published(published) = process.reload_with(&admit) else {
        panic!("a [database] edit must publish");
    };
    assert!(process.store().authorize(&published.revision()).is_ok());
    assert!(process.notes().database_restart);
}

/// The refusal names the file and the key path.
#[test]
fn startup_refusal_names_the_source_and_key() {
    let files = sources("picker = \"sideways\"\n");
    let host_reads = Cell::new(0);
    let Err(error) = ProcessAdmission::admit_initial(
        seed(),
        thegn_core::db::MigrationActor::Controller,
        &admit_from(&files, &host_reads),
    ) else {
        panic!("invalid enum refuses");
    };
    let text = error.to_string();
    assert!(text.contains("config file:"), "{text}");
    assert!(text.contains("picker"), "{text}");
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

/// When a concurrent reload wins the CAS, the loop still receives the
/// winning generation instead of silently keeping its stale copy.
#[test]
fn superseded_reload_still_delivers_the_current_generation_to_the_loop() {
    let files = sources("branch_prefix = \"one/\"\n");
    let host_reads = Cell::new(0);
    let admit = admit_from(&files, &host_reads);
    let (process, _) =
        ProcessAdmission::admit_initial(seed(), thegn_core::db::MigrationActor::Controller, &admit)
            .unwrap();
    files
        .bodies
        .borrow_mut()
        .insert(base_path(), b"branch_prefix = \"two/\"\n".to_vec());
    let ReloadOutcome::Published(_) = process.reload_with(&admit) else {
        panic!("publishes");
    };
    let delivered = loop_update(ReloadOutcome::Superseded, || process.store().display())
        .expect("superseded delivers")
        .expect("ok");
    assert_eq!(delivered.config().branch_prefix, "two/");
    assert!(loop_update(ReloadOutcome::FailedCoalesced, || process.store().display()).is_none());
}

/// A worktree removed under a live shell makes `current_dir` fail; that must
/// not become a lockout when nothing relative needs the cwd.
#[test]
fn an_unavailable_cwd_is_only_fatal_for_a_relative_config_path() {
    use crate::config_capture::{CaptureInputError, settle_cwd};
    let root = Path::new("/home/test/.thegn");
    assert_eq!(
        settle_cwd(Some(PathBuf::from("/cwd")), None, root).unwrap(),
        PathBuf::from("/cwd")
    );
    assert_eq!(settle_cwd(None, None, root).unwrap(), root);
    assert_eq!(
        settle_cwd(None, Some(Path::new("/absolute/config.toml")), root).unwrap(),
        root
    );
    assert_eq!(
        settle_cwd(None, Some(Path::new("relative.toml")), root),
        Err(CaptureInputError::CwdUnavailable)
    );
    assert_eq!(
        settle_cwd(None, None, Path::new("relative-root")),
        Err(CaptureInputError::CwdUnavailable)
    );
}

/// A write-rename race under the reader is retried once instead of refusing.
#[test]
fn a_changed_source_is_retried_once() {
    use crate::config_capture::{ConfigSourceReader, RetryOnChange};
    struct Flaky {
        reads: Cell<u32>,
    }
    impl ConfigSourceReader for Flaky {
        fn read_bounded(&self, _: &Path, _: usize) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() == 1 {
                Err(ConfigFileReadError::Changed)
            } else {
                Ok(Some(b"branch_prefix = \"settled/\"\n".to_vec()))
            }
        }
    }
    let reader = RetryOnChange(Flaky {
        reads: Cell::new(0),
    });
    assert!(reader.read_bounded(Path::new("/nonexistent"), 64).is_ok());

    struct AlwaysChanging;
    impl ConfigSourceReader for AlwaysChanging {
        fn read_bounded(&self, _: &Path, _: usize) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
            Err(ConfigFileReadError::Changed)
        }
    }
    assert_eq!(
        RetryOnChange(AlwaysChanging).read_bounded(Path::new("/nonexistent"), 64),
        Err(ConfigFileReadError::Changed)
    );
}
