//! Frozen process-source capture for the typed configuration admission edge.
//!
//! This module is intentionally standalone.  It captures ambient inputs once,
//! then admits one candidate through `thegn_core::config_admission`; it is not
//! startup or authority wiring.  Filesystem and SQLite reads are blocking and
//! must remain owned by a future off-loop worker.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thegn_core::config::{Config, EnvSource, PathExpansionContext};
use thegn_core::config_admission::{
    self, AdmissionDiagnostic, AdmittedConfig, ConfigAdmissionError, SourceContent, SourceInput,
};
use thegn_core::host_definition_snapshot::HostDefinitionsSnapshot;

const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFileReadError {
    Unavailable,
    NonRegular,
    Changed,
    TooLarge,
    InvalidUtf8,
    Unsupported,
}

impl std::fmt::Display for ConfigFileReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "configuration source could not be opened or read",
            Self::NonRegular => "configuration source is not a regular file",
            Self::Changed => "configuration source changed during capture",
            Self::TooLarge => "configuration source exceeds its byte limit",
            Self::InvalidUtf8 => "configuration source is not valid UTF-8",
            Self::Unsupported => "configuration source capture is unsupported on this platform",
        })
    }
}

/// The platform adapter owns the opened-file/no-follow policy.  It returns
/// bytes so invalid UTF-8 remains a typed admission failure instead of being
/// collapsed into a missing source.
pub(crate) trait ConfigSourceReader {
    fn read_bounded(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, ConfigFileReadError>;
}

pub(crate) struct CapturedCliInputs<'a> {
    pub(crate) config: Option<&'a Path>,
    pub(crate) overrides: &'a [String],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureInputError {
    CwdUnavailable,
    InvalidPath,
    Bounds,
    InvalidUtf8,
    UncapturedEnvironmentKey,
}

#[derive(Debug)]
pub(crate) enum CaptureFailure {
    Input(CaptureInputError),
    Source(ConfigFileReadError),
    Admission(ConfigAdmissionError),
    State(crate::state_host_capture::StateHostReadError),
}

impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omit paths, parser fragments, environment values and
        // host definitions from the rendered failure.
        f.write_str(match self {
            Self::Input(_) => "configuration input capture failed",
            Self::Source(error) => {
                return write!(f, "configuration source capture failed: {error}");
            }
            Self::Admission(error) => return write!(f, "configuration admission failed: {error}"),
            Self::State(_) => "configuration host-state capture failed",
        })
    }
}

impl std::error::Error for CaptureFailure {}

#[derive(Clone)]
struct CapturedSource {
    path: PathBuf,
    explicit: bool,
    /// Opaque path/selection digest consumed by core identity hashing.
    identity: String,
}

impl std::fmt::Debug for CapturedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedSource")
            .field("path", &"<redacted>")
            .field("explicit", &self.explicit)
            .field("identity", &"<redacted>")
            .finish()
    }
}

pub(crate) struct ConfigCaptureSeed {
    defaults: Config,
    user_home_path: PathBuf,
    paths: PathExpansionContext,
    source_cwd: PathBuf,
    config_home: PathBuf,
    state_home: PathBuf,
    app_root: PathBuf,
    profile_name: String,
    base: CapturedSource,
    profile: Option<CapturedSource>,
    state_db: PathBuf,
    env: FrozenEnv,
    overrides: Vec<String>,
}

impl std::fmt::Debug for ConfigCaptureSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigCaptureSeed")
            .field("source_cwd", &"<redacted>")
            .field("config_home", &"<redacted>")
            .field("state_home", &"<redacted>")
            .field("app_root", &"<redacted>")
            .field("profile_name", &self.profile_name)
            .field("base", &self.base)
            .field("profile", &self.profile)
            .field("state_db", &"<redacted>")
            .finish_non_exhaustive()
    }
}

pub(crate) struct CapturedConfig {
    admitted: AdmittedConfig,
    profile_name: String,
}

impl CapturedConfig {
    pub(crate) fn config(&self) -> &Config {
        self.admitted.config()
    }

    pub(crate) fn diagnostics(&self) -> &[AdmissionDiagnostic] {
        self.admitted.trace().diagnostics()
    }

    pub(crate) fn revision(&self) -> thegn_core::config_admission::ConfigRevision {
        self.admitted.revision()
    }

    pub(crate) fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub(crate) fn health(&self) -> thegn_core::config_admission::AdmissionHealth {
        self.admitted.health()
    }
}

struct RawCachingEnv<F> {
    read: F,
    values: RefCell<BTreeMap<String, Option<String>>>,
    bytes: Cell<usize>,
    error: Cell<Option<CaptureInputError>>,
}

impl<F: Fn(&str) -> Option<OsString>> RawCachingEnv<F> {
    fn new(read: F) -> Self {
        Self {
            read,
            values: RefCell::new(BTreeMap::new()),
            bytes: Cell::new(0),
            error: Cell::new(None),
        }
    }

    fn raw(&self, key: &str) -> Option<String> {
        if let Some(value) = self.values.borrow().get(key) {
            return value.clone();
        }
        if self.error.get().is_some() {
            return None;
        }
        if key.len() > thegn_core::config_budget::MAX_CONTEXT_BYTES
            || self.values.borrow().len() >= thegn_core::config_budget::MAX_ENV_ENTRIES
        {
            self.error.set(Some(CaptureInputError::Bounds));
            return None;
        }
        let raw = (self.read)(key);
        let value = match raw {
            None => None,
            Some(raw) => {
                let size = raw.as_encoded_bytes().len();
                let total = self
                    .bytes
                    .get()
                    .checked_add(key.len())
                    .and_then(|n| n.checked_add(size));
                if size > thegn_core::config_budget::MAX_ENV_VALUE_BYTES
                    || total.is_none_or(|n| n > thegn_core::config_budget::MAX_ENV_BYTES)
                {
                    self.error.set(Some(CaptureInputError::Bounds));
                    return None;
                }
                self.bytes
                    .set(total.expect("checked environment aggregate"));
                match raw.into_string() {
                    Ok(value) => Some(value),
                    Err(_) => {
                        self.error.set(Some(CaptureInputError::InvalidUtf8));
                        None
                    }
                }
            }
        };
        self.values
            .borrow_mut()
            .insert(key.to_owned(), value.clone());
        value
    }
}

impl<F: Fn(&str) -> Option<OsString>> EnvSource for RawCachingEnv<F> {
    fn get(&self, key: &str) -> Option<String> {
        self.raw(key).filter(|value| !value.trim().is_empty())
    }
}

struct FrozenEnv {
    values: BTreeMap<String, Option<String>>,
    missing: Cell<bool>,
}

impl EnvSource for FrozenEnv {
    fn get(&self, key: &str) -> Option<String> {
        match self.values.get(key) {
            Some(value) => value.clone(),
            None => {
                self.missing.set(true);
                None
            }
        }
    }
}

fn checked_path(path: &Path) -> Result<(), CaptureInputError> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES
        || path
            .to_str()
            .is_some_and(|value| value.chars().any(char::is_control))
    {
        Err(CaptureInputError::InvalidPath)
    } else {
        Ok(())
    }
}

fn settle(cwd: &Path, path: &Path) -> Result<PathBuf, CaptureInputError> {
    checked_path(path)?;
    let settled = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    checked_path(&settled)?;
    Ok(settled)
}

fn opaque_identity(role: &str, path: &Path, explicit: bool, profile: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"thegn/config-capture/source/v1\0");
    hasher.update(role.as_bytes());
    hasher.update([0]);
    hasher.update([u8::from(explicit)]);
    hasher.update([0]);
    hasher.update(profile.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let mut text = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

impl ConfigCaptureSeed {
    pub(crate) fn capture_process(cli: CapturedCliInputs<'_>) -> Result<Self, CaptureFailure> {
        if let Some(path) = cli.config {
            checked_path(path).map_err(CaptureFailure::Input)?;
        }
        let cwd = std::env::current_dir()
            .map_err(|_| CaptureFailure::Input(CaptureInputError::CwdUnavailable))?;
        Self::capture_with(cli, cwd, |key| std::env::var_os(key), Config::default)
            .map_err(CaptureFailure::Input)
    }

    pub(crate) fn capture_with(
        cli: CapturedCliInputs<'_>,
        cwd: PathBuf,
        read: impl Fn(&str) -> Option<OsString>,
        defaults: impl FnOnce() -> Config,
    ) -> Result<Self, CaptureInputError> {
        if let Some(path) = cli.config {
            checked_path(path)?;
        }
        if !cwd.is_absolute() {
            return Err(CaptureInputError::InvalidPath);
        }
        checked_path(&cwd)?;
        if cli.overrides.len() > thegn_core::config_budget::MAX_CLI_ENTRIES
            || cli
                .overrides
                .iter()
                .any(|value| value.len() > thegn_core::config_budget::MAX_CLI_ENTRY_BYTES)
            || cli
                .overrides
                .iter()
                .try_fold(0usize, |total, value| total.checked_add(value.len()))
                .is_none_or(|total| total > thegn_core::config_budget::MAX_CLI_BYTES)
        {
            return Err(CaptureInputError::Bounds);
        }

        let env = RawCachingEnv::new(read);
        let keys = crate::platform::config_file_capture::native_path_keys();
        let home = settle(
            &cwd,
            Path::new(
                &env.raw(keys.home)
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| keys.home_fallback.into()),
            ),
        )?;
        let config_home = settle(
            &cwd,
            &env.raw(keys.config)
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(keys.config_fallback)),
        )?;
        let state_home = settle(
            &cwd,
            &env.raw(keys.state)
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(keys.state_fallback)),
        )?;
        let app_root = settle(
            &cwd,
            &env.raw("THEGN_DIR")
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".thegn")),
        )?;

        // Populate the exact mapper-requested set once.  The admission call
        // later receives FrozenEnv and can therefore never consult ambient
        // process state a second time.
        let _ = thegn_core::config::env_overlay(&env);
        if let Some(error) = env.error.get() {
            return Err(error);
        }
        let profile_name =
            thegn_core::profile::normalize_name(&env.get("THEGN_PROFILE").unwrap_or_default());
        let base_path = settle(
            &cwd,
            &cli.config
                .map(Path::to_owned)
                .unwrap_or_else(|| config_home.join("thegn/config.toml")),
        )?;
        let base = CapturedSource {
            identity: opaque_identity("base", &base_path, cli.config.is_some(), &profile_name),
            path: base_path,
            explicit: cli.config.is_some(),
        };
        let profile = (profile_name != "default").then(|| {
            let path = config_home
                .join("thegn/profiles")
                .join(&profile_name)
                .join("config.toml");
            CapturedSource {
                identity: opaque_identity("profile", &path, true, &profile_name),
                path,
                explicit: true,
            }
        });
        let state_db = settle(&cwd, &state_home.join("thegn/thegn.db"))?;

        // Config::default consults ambient roots.  Replace every root it owns
        // with the already-captured values before any caller can mutate them.
        let mut defaults = defaults();
        defaults.worktrees_dir = app_root
            .join("worktrees")
            .to_str()
            .ok_or(CaptureInputError::InvalidUtf8)?
            .to_owned();
        defaults.workspaces_dir = home
            .join("code")
            .to_str()
            .ok_or(CaptureInputError::InvalidUtf8)?
            .to_owned();

        Ok(Self {
            defaults,
            user_home_path: home.clone(),
            paths: PathExpansionContext::from_home(home.clone()),
            source_cwd: cwd,
            config_home,
            state_home,
            app_root,
            profile_name,
            base,
            profile,
            state_db,
            env: FrozenEnv {
                values: env.values.into_inner(),
                missing: Cell::new(false),
            },
            overrides: cli.overrides.to_vec(),
        })
    }

    /// Blocking and deliberately unwired.  The state adapter decides whether
    /// an absent DB is genuine absence; all other DB failures remain fatal.
    pub(crate) fn load_once(&self) -> Result<CapturedConfig, CaptureFailure> {
        self.load_with(&crate::platform::config_file_capture::Reader, || {
            crate::state_host_capture::capture(&self.state_db)
        })
    }

    pub(crate) fn load_with(
        &self,
        reader: &dyn ConfigSourceReader,
        capture_state: impl FnOnce() -> Result<
            crate::state_host_capture::StateHostCapture,
            crate::state_host_capture::StateHostReadError,
        >,
    ) -> Result<CapturedConfig, CaptureFailure> {
        self.env.missing.set(false);
        let base = reader
            .read_bounded(&self.base.path, thegn_core::config_budget::MAX_SOURCE_BYTES)
            .map_err(CaptureFailure::Source)?;
        let profile = match &self.profile {
            Some(source) => Some(
                reader
                    .read_bounded(&source.path, thegn_core::config_budget::MAX_SOURCE_BYTES)
                    .map_err(CaptureFailure::Source)?,
            ),
            None => None,
        };
        let base_input = SourceInput {
            identity: &self.base.identity,
            explicit: self.base.explicit,
            content: SourceContent::Bytes(base.as_deref().unwrap_or_default()),
        };
        // An absent implicit base must stay Absent, not become an empty byte
        // source: the core admission health distinguishes first-run defaults.
        let base_input = if base.is_some() {
            base_input
        } else {
            SourceInput::absent(&self.base.identity, self.base.explicit)
        };
        let profile_input = self.profile.as_ref().map(|source| SourceInput {
            identity: &source.identity,
            explicit: source.explicit,
            content: SourceContent::Bytes(profile.as_deref().unwrap_or_default()),
        });
        let profile_input = match (profile_input, profile.as_ref()) {
            (Some(input), Some(_)) => Some(input),
            (Some(input), None) => Some(SourceInput::absent(input.identity, input.explicit)),
            _ => None,
        };
        // Validate all non-DB layers before touching SQLite.  The empty
        // snapshot is only a preflight input and is never published.
        let empty_hosts = HostDefinitionsSnapshot::empty(thegn_core::db::SCHEMA_VERSION);
        config_admission::admit(config_admission::AdmissionInputs {
            defaults: self.defaults.clone(),
            base: base_input,
            profile: profile_input,
            env: &self.env,
            overrides: &self.overrides,
            hosts: &empty_hosts,
            paths: &self.paths,
        })
        .map_err(CaptureFailure::Admission)?;
        if self.env.missing.get() {
            return Err(CaptureFailure::Input(
                CaptureInputError::UncapturedEnvironmentKey,
            ));
        }
        let hosts = match capture_state().map_err(CaptureFailure::State)? {
            crate::state_host_capture::StateHostCapture::Absent => {
                HostDefinitionsSnapshot::empty(thegn_core::db::SCHEMA_VERSION)
            }
            crate::state_host_capture::StateHostCapture::Present(snapshot) => snapshot,
        };
        let admitted = config_admission::admit(config_admission::AdmissionInputs {
            defaults: self.defaults.clone(),
            base: base_input,
            profile: profile_input,
            env: &self.env,
            overrides: &self.overrides,
            hosts: &hosts,
            paths: &self.paths,
        })
        .map_err(CaptureFailure::Admission)?;
        if self.env.missing.get() {
            return Err(CaptureFailure::Input(
                CaptureInputError::UncapturedEnvironmentKey,
            ));
        }
        Ok(CapturedConfig {
            admitted,
            profile_name: self.profile_name.clone(),
        })
    }
}

#[cfg(test)]
#[path = "config_capture_tests.rs"]
mod tests;
