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
    FinalLinkUnsupported,
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
            Self::FinalLinkUnsupported => {
                "configuration file is a final symlink/reparse point; select a reviewed ordinary private file (copying a managed config stops automatic tracking)"
            }
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
    /// Raw CLI selector.  It is resolved with CLI-over-environment precedence
    /// and must not be reread by the capture worker.
    pub(crate) profile: Option<&'a str>,
    /// When the single-threaded startup adapter has already called
    /// `profile::reroot`, pass its frozen result here.  This prevents capture
    /// from deriving a second `profiles/<name>` root from the already-rerooted
    /// `THEGN_DIR`.  `None` is valid only for the pre-reroot/pure-capture
    /// contract; it derives the same profile paths without mutating the
    /// process environment.
    pub(crate) profile_paths: Option<&'a thegn_core::profile::ProfilePaths>,
    pub(crate) overrides: &'a [String],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureInputError {
    CwdUnavailable,
    InvalidPath,
    Bounds,
    InvalidUtf8,
    ProfileBindingMismatch,
    ProfileSelector(thegn_core::profile::ProfileSelectorError),
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
        // `reroot` is deliberately not called here.  A future single-threaded
        // adapter calls it before this function and its immutable result is
        // used to prevent a second reroot; tests may use the pure fallback in
        // `capture_with` before reroot.
        let active = thegn_core::profile::active();
        let cli = CapturedCliInputs {
            config: cli.config,
            profile: cli.profile,
            profile_paths: Some(&active),
            overrides: cli.overrides,
        };
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
        if cli
            .profile
            .is_some_and(|value| value.len() > thegn_core::config_budget::MAX_CONTEXT_BYTES)
            || cli.profile_paths.is_some_and(|paths| {
                paths.name.len() > thegn_core::config_budget::MAX_CONTEXT_BYTES
            })
        {
            return Err(CaptureInputError::Bounds);
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
        let env_profile = env.raw("THEGN_PROFILE");
        let _ = thegn_core::config::env_overlay(&env);
        if let Some(error) = env.error.get() {
            return Err(error);
        }
        let base_app_root = app_root.clone();
        // Validate the selected process profile even when a caller supplies a
        // pre-rerooted path. This preserves CLI-over-environment precedence
        // and prevents an invalid raw selector from being hidden by a frozen
        // path identity.
        let selector = thegn_core::profile::resolve_selector(cli.profile, env_profile.as_deref())
            .map_err(CaptureInputError::ProfileSelector)?;
        let resolved_profile = if let Some(paths) = cli.profile_paths {
            // Reject the complete retained root before cloning it into the
            // seed.  This is also the no-double-reroot path.
            checked_path(&paths.root)?;
            paths.clone()
        } else {
            let paths = thegn_core::profile::resolve_for_capture(
                &base_app_root,
                cli.profile,
                env_profile.as_deref(),
            )
            .map_err(CaptureInputError::ProfileSelector)?;
            checked_path(&paths.root)?;
            paths
        };
        let requested_name = selector.name;
        let requested_capped = thegn_core::profile::cap_name(
            &requested_name,
            thegn_core::profile::MAX_NEW_PROFILE_NAME,
        );
        if resolved_profile
            .root
            .as_os_str()
            .as_encoded_bytes()
            .is_empty()
            || !resolved_profile.root.is_absolute()
            || (requested_name == "default" && resolved_profile.name != "default")
            || (requested_name != "default"
                && resolved_profile.name != requested_name
                && resolved_profile.name != requested_capped)
        {
            return Err(CaptureInputError::ProfileBindingMismatch);
        }
        let profile_name = resolved_profile.name.clone();
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
        let profile = if profile_name != "default" {
            let path = config_home
                .join("thegn/profiles")
                .join(&profile_name)
                .join("config.toml");
            checked_path(&path)?;
            Some(CapturedSource {
                identity: opaque_identity("profile", &path, true, &profile_name),
                path,
                explicit: true,
            })
        } else {
            None
        };
        let selected_state_home = if profile_name == "default" {
            state_home.clone()
        } else {
            resolved_profile.root.join("state")
        };
        checked_path(&selected_state_home)?;
        let state_db = settle(&cwd, &selected_state_home.join("thegn/thegn.db"))?;

        // Config::default consults ambient roots.  Replace every root it owns
        // with the already-captured values before any caller can mutate them.
        let mut defaults = defaults();
        let worktrees_root = resolved_profile.root.join("worktrees");
        checked_path(&worktrees_root)?;
        defaults.worktrees_dir = worktrees_root
            .to_str()
            .ok_or(CaptureInputError::InvalidUtf8)?
            .to_owned();
        defaults.workspaces_dir = home
            .join("code")
            .to_str()
            .ok_or(CaptureInputError::InvalidUtf8)?
            .to_owned();

        let mut frozen_values = env.values.into_inner();
        if cli.profile.is_some() || env_profile.is_some() || profile_name != "default" {
            // The selector is a captured process input, not an ambient
            // mutation.  Pin it in the frozen mapper view so CLI-over-env
            // selection cannot be undone by config admission's env overlay.
            frozen_values.insert("THEGN_PROFILE".to_owned(), Some(profile_name.clone()));
        }

        Ok(Self {
            defaults,
            user_home_path: home.clone(),
            paths: PathExpansionContext::from_home(home.clone()),
            source_cwd: cwd,
            config_home,
            state_home: selected_state_home,
            app_root: resolved_profile.root,
            profile_name,
            base,
            profile,
            state_db,
            env: FrozenEnv {
                values: frozen_values,
                missing: Cell::new(false),
            },
            overrides: cli.overrides.to_vec(),
        })
    }

    /// The selected profile is part of the frozen capture contract.  The
    /// profile file is read once when selected; `Some(None)` means that the
    /// selected file is absent and must become `SourceInput::Absent`, while
    /// `Some(Some(Vec::new()))` is a present empty overlay.
    fn source_input<'a>(source: &'a CapturedSource, content: Option<&'a [u8]>) -> SourceInput<'a> {
        match content {
            Some(content) => SourceInput {
                identity: &source.identity,
                explicit: source.explicit,
                content: SourceContent::Bytes(content),
            },
            None => SourceInput::absent(&source.identity, source.explicit),
        }
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
        let profile_content = profile.as_ref().map(|content| content.as_deref());
        let profile_input = match (&self.profile, profile_content) {
            (Some(source), Some(content)) => Some(Self::source_input(source, content)),
            (Some(source), None) => Some(Self::source_input(source, None)),
            (None, None) => None,
            (None, Some(_)) => unreachable!("profile content without a selected profile"),
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
